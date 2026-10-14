//! Infernix inference engine adapter.
//!
//! Infernix (`infernix-serve`, a C++/CUDA engine forked from NInfer — not a
//! llama.cpp derivative) exposes standard Prometheus exposition at `/metrics`
//! under an `infernix_` prefix, including three real latency histograms. That
//! makes this adapter the closest relative of the vLLM one: parse, map, and
//! interpolate percentiles from buckets. No `/slots` scraping, no log scraping.
//!
//! Engine-side facts carried by the mapping:
//! * Infernix already subtracts its own attach-time baseline from every
//!   `*_total` it renders (`Metrics::render`'s `counter()` lambda), so its
//!   counters mean "since the service attached" — already the window the
//!   dashboard cares about. The warmup tracker still layers a baseline of its
//!   own on top, because the dashboard attaches whenever it likes, and the
//!   engine's first served request is a CUDA-graph-capture outlier like every
//!   other engine's.
//! * KV cache usage is a real gauge pair (`device_kv_used_pages` /
//!   `device_kv_capacity_pages`), so `kv_cache_is_estimated` is false — unlike
//!   Strata, which has no such gauge.
//! * There is no inter-token or time-per-output-token histogram, so inter-token
//!   latency is derived: a request's decode time is its duration minus its time
//!   to first token, and the tokens in that span are counted by
//!   `decode_tokens_total` (committed decode tokens, first token excluded). It
//!   is a mean over settled requests and it slightly overestimates, because
//!   duration runs through settlement and so includes the post-token tail.
//!   TPOT and its percentiles/goodput stay `None` — there is no second source,
//!   and inventing one would only mirror the ITL tile back at the operator.
//! * `swapped_requests` is fed by `requests_paused` (requests parked awaiting
//!   recovery), the engine's memory-pressure signal; vLLM's counter of that
//!   name counts the same condition.
//! * Speculative decoding: Infernix counts verification *rounds* where vLLM
//!   counts draft attempts, so rounds feed `spec_decode_drafts_total` and mean
//!   acceptance length means "accepted tokens per round".

use super::histogram::{fraction_le, percentile};
use super::prometheus::{parse_prometheus_text, ParsedMetrics};
use super::vllm::{spec_acceptance_rate, spec_mean_acceptance_length, warmup_skip_from_env};
use super::warmup::WarmupTracker;
use super::{
    EngineAdapter, EngineMetrics, EngineStatus, EngineType, HistogramBucket, LatencyPercentiles,
    MetricGroup, ModelInfo, ModelMetadataError, ModelResolution, E2E_SLO_MS, TTFT_SLO_MS,
};
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Histograms. Infernix emits `_bucket` / `_sum` / `_count` for each.
const H_TTFT: &str = "infernix_time_to_first_token_seconds";
const H_E2E: &str = "infernix_request_duration_seconds";
const H_QUEUE: &str = "infernix_request_queue_seconds";
/// The warmup cursor: one observation per settled request, like vLLM's.
const C_TTFT_COUNT: &str = "infernix_time_to_first_token_seconds_count";

/// Gauges.
const G_RUNNING: &str = "infernix_requests_running";
const G_WAITING: &str = "infernix_requests_waiting";
const G_PAUSED: &str = "infernix_requests_paused";
const G_KV_USED: &str = "infernix_device_kv_used_pages";
const G_KV_CAPACITY: &str = "infernix_device_kv_capacity_pages";

/// Counters (already attach-baselined by the engine).
const C_PROMPT_TOKENS: &str = "infernix_prompt_tokens_total";
const C_PROMPT_CACHED: &str = "infernix_prompt_tokens_cached_total";
const C_GENERATION_TOKENS: &str = "infernix_generation_tokens_total";
const C_DECODE_TOKENS: &str = "infernix_decode_tokens_total";
const C_DECODE_ROUNDS: &str = "infernix_decode_rounds_total";
const C_DECODE_ROW_ROUNDS: &str = "infernix_decode_row_rounds_total";
const C_PREEMPTIONS: &str = "infernix_preemptions_total";
const C_SPEC_ROUNDS: &str = "infernix_spec_decode_rounds_total";
const C_SPEC_DRAFT_TOKENS: &str = "infernix_spec_decode_draft_tokens_total";
const C_SPEC_ACCEPTED_TOKENS: &str = "infernix_spec_decode_accepted_tokens_total";

pub struct InfernixAdapter {
    client: reqwest::Client,
    endpoint: String,
    /// Bearer token for a `--api-key` deployment. Infernix leaves `/health`
    /// open but gates `/metrics` and `/v1/models`, so both carry it.
    api_key: Option<String>,
    /// Model identity hint from the launch command line, used only when
    /// `/v1/models` cannot be read.
    served_model: Option<String>,
    prev_gen_tokens: Mutex<Option<(f64, Instant)>>,
    prev_prompt_tokens: Mutex<Option<(f64, Instant)>>,
    prev_spec_decode: Mutex<Option<(f64, f64)>>,
    avg_accum: Mutex<(f64, u32)>,
    avg_prompt_accum: Mutex<(f64, u32)>,
    warmup: Mutex<WarmupTracker>,
}

impl InfernixAdapter {
    pub fn new(
        client: reqwest::Client,
        endpoint: String,
        served_model: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client,
            endpoint,
            api_key,
            served_model,
            prev_gen_tokens: Mutex::new(None),
            prev_prompt_tokens: Mutex::new(None),
            prev_spec_decode: Mutex::new(None),
            avg_accum: Mutex::new((0.0, 0)),
            avg_prompt_accum: Mutex::new((0.0, 0)),
            warmup: Mutex::new(WarmupTracker::new(warmup_skip_from_env(), C_TTFT_COUNT)),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    /// The command-line hint, used as a fallback model name. Infernix ids carry
    /// no HF org prefix, so there is nothing to enrich — it is shown as given.
    fn fallback_model(&self) -> Option<ModelInfo> {
        self.served_model
            .as_deref()
            .map(|name| model_info(name.to_string()))
    }
}

fn model_info(name: String) -> ModelInfo {
    ModelInfo {
        name,
        parameter_size: None,
        quantization: None,
        precision: None,
        tensor_type: None,
        model_type: None,
        pipeline_tag: None,
    }
}

/// Everything `/metrics` yields on its own — no per-poll rate state. Split out
/// so the mapping is testable against a captured exposition body with no
/// running engine.
///
/// `parsed` carries the warmup tracker's baseline-subtracted counters, which is
/// what rates, averages and percentiles must be computed over; `raw` carries
/// the absolute values for the pass-through display fields, so those stay
/// continuous across the warmup transition. Same division as the vLLM adapter.
///
/// `blank` is the warmup flag: while warming, every derived value would carry
/// the first-request outlier, so it is left `None` here rather than corrected
/// by the caller. Gauges, lifetime totals and the prefix-cache rate stay
/// populated so the UI still shows live engine state.
fn map_metrics(parsed: &ParsedMetrics, raw: &ParsedMetrics, blank: bool) -> EngineMetrics {
    let counter = |m: &ParsedMetrics, key: &str| m.counters.get(key).copied();
    let gauge = |m: &ParsedMetrics, key: &str| m.gauges.get(key).copied();
    // A derived value is either wanted or withheld for warmup; saying it once
    // here keeps the warmup contract out of the assembly below.
    let derived = |v: Option<f64>| if blank { None } else { v };

    let active_requests = gauge(parsed, G_RUNNING).map(|v| v.max(0.0) as u64);
    let queued_requests = gauge(parsed, G_WAITING).map(|v| v.max(0.0) as u64);
    let swapped_requests = gauge(parsed, G_PAUSED).map(|v| v.max(0.0) as u64);

    // KV cache: a real used/capacity page pair, so the percentage is measured,
    // not estimated. Zero or absent capacity leaves the tile blank.
    let kv_cache_percent = match (gauge(parsed, G_KV_USED), gauge(parsed, G_KV_CAPACITY)) {
        (Some(used), Some(cap)) if cap > 0.0 => Some((used / cap) * 100.0),
        _ => None,
    };

    // One TTFT observation per settled request, so the histogram count is the
    // request counter. The engine's own `requests_total` carries an `outcome`
    // label, and the Prometheus parser keys samples by name alone — reading it
    // would report one arbitrary outcome as the total.
    let total_requests = counter(raw, C_TTFT_COUNT).map(|v| v.max(0.0) as u64);

    let lifetime_prompt_tokens = counter(raw, C_PROMPT_TOKENS);
    let lifetime_generation_tokens = counter(raw, C_GENERATION_TOKENS);

    // Prefix cache: input tokens served from exact checkpoints over all input
    // tokens bound — a token-level hit rate, same shape as vLLM's pair.
    let prefix_cache_hit_rate = match (
        counter(parsed, C_PROMPT_CACHED),
        counter(parsed, C_PROMPT_TOKENS),
    ) {
        (Some(cached), Some(total)) if total > 0.0 => Some((cached / total) * 100.0),
        _ => None,
    };
    let prefix_cache_queries_total = counter(raw, C_PROMPT_TOKENS).map(|v| v.max(0.0) as u64);

    let preemptions_total = counter(raw, C_PREEMPTIONS).map(|v| v.max(0.0) as u64);

    // Average tokens per decode batch = sum of batch sizes / batch executions.
    let avg_batch_size = derived(
        match (
            counter(parsed, C_DECODE_ROW_ROUNDS),
            counter(parsed, C_DECODE_ROUNDS),
        ) {
            (Some(rows), Some(rounds)) if rounds > 0.0 => Some(rows / rounds),
            _ => None,
        },
    );

    // Mean latencies from histogram sum/count.
    let mean_ms = |base: &str| -> Option<f64> {
        let sum = counter(parsed, &format!("{base}_sum"))?;
        let count = counter(parsed, &format!("{base}_count"))?;
        if count > 0.0 {
            Some((sum / count) * 1000.0)
        } else {
            None
        }
    };
    let ttft_ms = derived(mean_ms(H_TTFT));
    let e2e_latency_ms = derived(mean_ms(H_E2E));
    let queue_time_ms = derived(mean_ms(H_QUEUE));

    // Decode-only time over settled requests: total request time minus
    // first-token time. Negative when a request's TTFT fell in this window but
    // its completion did not — that is not a decode span.
    let decode_seconds = match (
        counter(parsed, &format!("{H_E2E}_sum")),
        counter(parsed, &format!("{H_TTFT}_sum")),
    ) {
        (Some(duration), Some(ttft)) if duration > ttft => Some(duration - ttft),
        _ => None,
    };

    let inter_token_latency_ms =
        derived(match (decode_seconds, counter(parsed, C_DECODE_TOKENS)) {
            (Some(seconds), Some(tokens)) if tokens > 0.0 => Some((seconds / tokens) * 1000.0),
            _ => None,
        });

    // Per-request throughputs: output tokens over the decode span, prompt
    // tokens over the prefill span (approximated by TTFT, as in vllm.rs).
    let per_request_tps = derived(
        match (counter(parsed, C_GENERATION_TOKENS), decode_seconds) {
            (Some(tokens), Some(seconds)) if seconds > 0.0 => Some(tokens / seconds),
            _ => None,
        },
    );
    let per_request_prompt_tps = derived(
        match (
            counter(parsed, C_PROMPT_TOKENS),
            counter(parsed, &format!("{H_TTFT}_sum")),
        ) {
            (Some(tokens), Some(seconds)) if seconds > 0.0 => Some(tokens / seconds),
            _ => None,
        },
    );

    let percentiles_ms = |metric: &str| -> Option<LatencyPercentiles> {
        let buckets = parsed.histograms.get(metric)?;
        let to_ms = |q: f64| percentile(buckets, q).map(|s| s * 1000.0);
        let p = LatencyPercentiles {
            p50_ms: to_ms(0.50),
            p95_ms: to_ms(0.95),
            p99_ms: to_ms(0.99),
        };
        if p.p50_ms.is_none() && p.p95_ms.is_none() && p.p99_ms.is_none() {
            None
        } else {
            Some(p)
        }
    };
    let goodput_pct = |metric: &str, slo_ms: f64| -> Option<f64> {
        let buckets = parsed.histograms.get(metric)?;
        fraction_le(buckets, slo_ms / 1000.0).map(|f| f * 100.0)
    };
    let buckets_for = |metric: &str| -> Option<Vec<HistogramBucket>> {
        let raw_buckets = parsed.histograms.get(metric)?;
        if raw_buckets.is_empty() {
            return None;
        }
        Some(
            raw_buckets
                .iter()
                .map(|&(le, cumulative_count)| HistogramBucket {
                    // serde_json cannot encode +Inf; the frontend's
                    // `fraction_le` port reads this sentinel as the overflow
                    // bucket (same convention as vllm.rs).
                    le_seconds: if le.is_finite() { le } else { f64::MAX },
                    cumulative_count,
                })
                .collect(),
        )
    };

    // Speculative decoding. Present only when a speculative backend is
    // configured; their absence is what hides the section in the UI, so
    // lifetime values are read absolute and ungated by warmup.
    let spec_rounds = counter(raw, C_SPEC_ROUNDS);
    let spec_draft = counter(raw, C_SPEC_DRAFT_TOKENS);
    let spec_accepted = counter(raw, C_SPEC_ACCEPTED_TOKENS);

    EngineMetrics {
        active_requests,
        queued_requests,
        swapped_requests,
        kv_cache_percent,
        kv_cache_is_estimated: false,
        total_requests,
        total_prompt_tokens: lifetime_prompt_tokens.map(|v| v.max(0.0) as u64),
        total_generation_tokens: lifetime_generation_tokens.map(|v| v.max(0.0) as u64),
        prefix_cache_hit_rate,
        prefix_cache_queries_total,
        preemptions_total,
        avg_batch_size,
        ttft_ms,
        e2e_latency_ms,
        queue_time_ms,
        inter_token_latency_ms,
        per_request_tps,
        per_request_prompt_tps,
        ttft_percentiles: if blank { None } else { percentiles_ms(H_TTFT) },
        e2e_percentiles: if blank { None } else { percentiles_ms(H_E2E) },
        ttft_goodput_pct: if blank {
            None
        } else {
            goodput_pct(H_TTFT, TTFT_SLO_MS)
        },
        e2e_goodput_pct: if blank {
            None
        } else {
            goodput_pct(H_E2E, E2E_SLO_MS)
        },
        ttft_buckets: if blank { None } else { buckets_for(H_TTFT) },
        e2e_buckets: if blank { None } else { buckets_for(H_E2E) },
        spec_decode_draft_tokens_total: spec_draft.map(|v| v.max(0.0) as u64),
        spec_decode_accepted_tokens_total: spec_accepted.map(|v| v.max(0.0) as u64),
        spec_decode_drafts_total: spec_rounds.map(|v| v.max(0.0) as u64),
        spec_decode_acceptance_rate: spec_acceptance_rate(spec_accepted, spec_draft),
        spec_decode_mean_acceptance_length: spec_mean_acceptance_length(spec_accepted, spec_rounds),
        // Filled by `get_metrics`: per-poll rates and the live acceptance rate
        // need previous-tick state, and `warming_up` belongs to the tracker.
        ..EngineMetrics::default()
    }
}

#[async_trait]
impl EngineAdapter for InfernixAdapter {
    fn engine_type(&self) -> EngineType {
        EngineType::Infernix
    }

    /// See the note on `LlamaCppAdapter::metric_groups`. Infernix gates nothing
    /// in the capability table either, so all-on is also what it truly reports.
    fn metric_groups(&self) -> BTreeSet<MetricGroup> {
        MetricGroup::all()
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn health_check(&self) -> EngineStatus {
        // `/health` answers 200 while the engine can take work and 503 when it
        // cannot, and it is never auth-gated — so no token goes here.
        match self
            .client
            .get(format!("{}/health", self.endpoint))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => EngineStatus::Running,
            Ok(r) => EngineStatus::Error(format!("HTTP {}", r.status())),
            Err(e) => EngineStatus::Error(e.to_string()),
        }
    }

    async fn get_model_info(&self) -> ModelResolution {
        // OpenAI-shaped `/v1/models` whose single id is Infernix's public model
        // id — the artifact basename, or `--model-id` when the operator gave
        // one. `--api-key` gates this path, and that rejection has to reach the
        // operator rather than silently degrade to a fallback (issue #90).
        let reply = match self
            .auth(
                self.client
                    .get(format!("{}/v1/models", self.endpoint))
                    .timeout(Duration::from_secs(2)),
            )
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<OpenAIModelsReply>().await {
                    Ok(m) => m.data.first().map(|m| m.id.clone()),
                    Err(_) => None,
                }
            }
            Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    status = %resp.status(),
                    "/v1/models requires an API key",
                );
                return ModelResolution {
                    model: self.fallback_model(),
                    metadata_error: Some(ModelMetadataError::AuthRequired),
                };
            }
            Ok(resp) => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    status = %resp.status(),
                    "/v1/models returned non-success",
                );
                return ModelResolution {
                    model: self.fallback_model(),
                    metadata_error: Some(ModelMetadataError::Unavailable),
                };
            }
            Err(e) => {
                tracing::debug!(endpoint = %self.endpoint, error = %e, "/v1/models request failed");
                return ModelResolution {
                    model: self.fallback_model(),
                    metadata_error: Some(ModelMetadataError::Unavailable),
                };
            }
        };

        match reply {
            Some(id) if !id.is_empty() => ModelResolution {
                model: Some(model_info(id)),
                metadata_error: None,
            },
            // An empty model list is as unusable as no answer at all.
            _ => ModelResolution {
                model: self.fallback_model(),
                metadata_error: Some(ModelMetadataError::Unavailable),
            },
        }
    }

    async fn get_metrics(&self) -> Option<EngineMetrics> {
        let body = self
            .auth(
                self.client
                    .get(format!("{}/metrics", self.endpoint))
                    .timeout(Duration::from_secs(2)),
            )
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;

        let raw = parse_prometheus_text(&body)?;
        let warmup_out = {
            let mut tracker = self.warmup.lock().await;
            tracker.observe(&raw)
        };
        // On the baseline transition the per-poll rate state still refers to
        // absolute counter values, while the tracker starts handing back deltas
        // — a stale reading would yield a hugely negative rate next tick.
        if warmup_out.just_transitioned {
            *self.prev_gen_tokens.lock().await = None;
            *self.prev_prompt_tokens.lock().await = None;
            *self.prev_spec_decode.lock().await = None;
            *self.avg_accum.lock().await = (0.0, 0);
            *self.avg_prompt_accum.lock().await = (0.0, 0);
            tracing::info!(
                endpoint = %self.endpoint,
                "warmup complete — baseline captured, steady-state metrics now reported"
            );
        }
        let parsed = &warmup_out.adjusted;
        let warming_up = warmup_out.warming_up;

        let mut metrics = map_metrics(parsed, &raw, warming_up);
        let now = Instant::now();

        // Windowed rates from counter deltas — same shape as the vLLM adapter.
        let rate_from = |current: Option<f64>,
                         prev: &mut Option<(f64, Instant)>,
                         now: Instant|
         -> Option<f64> {
            let rate = match (current, prev.as_ref()) {
                (Some(current), Some(&(prev_val, prev_time))) => {
                    let elapsed = now.duration_since(prev_time).as_secs_f64();
                    if elapsed > 0.0 {
                        Some((current - prev_val) / elapsed)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(val) = current {
                *prev = Some((val, now));
            }
            rate
        };

        let tokens_per_sec = {
            let mut prev = self.prev_gen_tokens.lock().await;
            rate_from(
                parsed.counters.get(C_GENERATION_TOKENS).copied(),
                &mut prev,
                now,
            )
        };
        let prompt_tokens_per_sec = {
            let mut prev = self.prev_prompt_tokens.lock().await;
            rate_from(
                parsed.counters.get(C_PROMPT_TOKENS).copied(),
                &mut prev,
                now,
            )
        };

        // Averages accumulate only over ticks that actually had throughput, so
        // an idle engine keeps its last steady-state figure instead of decaying
        // toward zero.
        let average = |rate: Option<f64>, accum: &mut (f64, u32)| -> Option<f64> {
            if let Some(rate) = rate {
                if rate > 0.0 {
                    accum.0 += rate;
                    accum.1 += 1;
                }
            }
            if accum.1 > 0 {
                Some(accum.0 / accum.1 as f64)
            } else {
                None
            }
        };
        let avg_tokens_per_sec = {
            let mut accum = self.avg_accum.lock().await;
            average(tokens_per_sec, &mut accum)
        };
        let avg_prompt_tokens_per_sec = {
            let mut accum = self.avg_prompt_accum.lock().await;
            average(prompt_tokens_per_sec, &mut accum)
        };

        // Live token acceptance rate from per-poll deltas. The snapshot is
        // dropped when a counter is missing or went backwards (an engine
        // restart resets its counters), either of which would otherwise diff
        // against a stale value and silently inflate or negate the rate.
        let spec_decode_acceptance_rate_live = {
            let mut prev = self.prev_spec_decode.lock().await;
            let accepted = parsed.counters.get(C_SPEC_ACCEPTED_TOKENS).copied();
            let draft = parsed.counters.get(C_SPEC_DRAFT_TOKENS).copied();
            match (accepted, draft) {
                (Some(acc), Some(dft)) => {
                    let live = prev.as_ref().and_then(|&(prev_acc, prev_draft)| {
                        if acc >= prev_acc && dft >= prev_draft {
                            spec_acceptance_rate(Some(acc - prev_acc), Some(dft - prev_draft))
                        } else {
                            None
                        }
                    });
                    *prev = Some((acc, dft));
                    live
                }
                _ => {
                    *prev = None;
                    None
                }
            }
        };

        // Rates are delta-derived, so they are withheld during warmup like the
        // rest of the derived group `map_metrics` already blanked.
        metrics.tokens_per_sec = if warming_up { None } else { tokens_per_sec };
        metrics.prompt_tokens_per_sec = if warming_up {
            None
        } else {
            prompt_tokens_per_sec
        };
        metrics.avg_tokens_per_sec = if warming_up { None } else { avg_tokens_per_sec };
        metrics.avg_prompt_tokens_per_sec = if warming_up {
            None
        } else {
            avg_prompt_tokens_per_sec
        };
        metrics.spec_decode_acceptance_rate_live = if warming_up {
            None
        } else {
            spec_decode_acceptance_rate_live
        };
        metrics.warming_up = warming_up;

        Some(metrics)
    }
}

#[derive(serde::Deserialize)]
struct OpenAIModelsReply {
    #[serde(default)]
    data: Vec<OpenAIModel>,
}

#[derive(serde::Deserialize)]
struct OpenAIModel {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/metrics` body in Infernix's exposition shape: TYPE lines, the
    /// `outcome`-labelled request counter, the labelled `model_info` gauge, and
    /// cumulative histogram buckets whose `le` bounds are all real members of
    /// the engine's `kBuckets` table. The three histogram arguments are each
    /// `(observations, total seconds)`. Trimmed to the samples the assertions
    /// need, never re-shaped.
    fn body(
        counters: &[(&str, f64)],
        gauges: &[(&str, f64)],
        ttft: (f64, f64),
        e2e: (f64, f64),
        queue: (f64, f64),
    ) -> String {
        let mut s = String::new();
        for (name, v) in counters {
            s.push_str(&format!(
                "# TYPE infernix_{name} counter\ninfernix_{name} {v}\n"
            ));
        }
        for (name, v) in gauges {
            s.push_str(&format!(
                "# TYPE infernix_{name} gauge\ninfernix_{name} {v}\n"
            ));
        }
        s.push_str(
            "# TYPE infernix_requests_total counter\n\
             infernix_requests_total{outcome=\"completed\"} 9\n\
             infernix_requests_total{outcome=\"cancelled\"} 1\n\
             # TYPE infernix_model_info gauge\n\
             infernix_model_info{model_name=\"qwen38\",speculative_backend=\"mtp\"} 1\n",
        );
        // Each histogram arrives as (observations, total seconds). Bucket counts
        // are cumulative and the last bucket is +Inf, as `Metrics::render`
        // emits them; the `le` bounds are real members of its `kBuckets` table.
        for (base, count, sum) in [
            (H_TTFT, ttft.0, ttft.1),
            (H_E2E, e2e.0, e2e.1),
            (H_QUEUE, queue.0, queue.1),
        ] {
            let (le, cum): (&[&str], Vec<f64>) = match base {
                H_TTFT => (
                    &["0.1", "1.0", "+Inf"],
                    vec![count * 0.4, count * 0.9, count],
                ),
                H_E2E => (
                    &["1.0", "5.0", "+Inf"],
                    vec![count * 0.2, count * 0.8, count],
                ),
                _ => (&["0.025", "0.1", "+Inf"], vec![count * 0.5, count, count]),
            };
            s.push_str(&format!("# TYPE {base} histogram\n"));
            for (bound, c) in le.iter().zip(cum) {
                s.push_str(&format!("{base}_bucket{{le=\"{bound}\"}} {c}\n"));
            }
            s.push_str(&format!("{base}_sum {sum}\n{base}_count {count}\n"));
        }
        s
    }

    fn mapped(counters: &[(&str, f64)], gauges: &[(&str, f64)]) -> EngineMetrics {
        mapped_with(counters, gauges, (10.0, 20.0), (10.0, 40.0), (10.0, 0.5))
    }

    fn mapped_with(
        counters: &[(&str, f64)],
        gauges: &[(&str, f64)],
        ttft: (f64, f64),
        e2e: (f64, f64),
        queue: (f64, f64),
    ) -> EngineMetrics {
        let text = body(counters, gauges, ttft, e2e, queue);
        let parsed = parse_prometheus_text(&text).expect("parse");
        map_metrics(&parsed, &parsed, false)
    }

    /// The engine's own attach baseline means its counters are already windowed,
    /// and the KV page pair is a measurement rather than an estimate.
    #[test]
    fn maps_gauges_counters_and_kv_pages() {
        let m = mapped(
            &[
                ("prompt_tokens_total", 1000.0),
                ("prompt_tokens_cached_total", 400.0),
                ("generation_tokens_total", 500.0),
                ("decode_tokens_total", 490.0),
                ("decode_rounds_total", 100.0),
                ("decode_row_rounds_total", 250.0),
                ("preemptions_total", 3.0),
            ],
            &[
                ("requests_running", 2.0),
                ("requests_waiting", 4.0),
                ("requests_paused", 1.0),
                ("device_kv_used_pages", 25.0),
                ("device_kv_capacity_pages", 100.0),
            ],
        );

        assert_eq!(m.active_requests, Some(2));
        assert_eq!(m.queued_requests, Some(4));
        assert_eq!(m.swapped_requests, Some(1));
        assert_eq!(m.kv_cache_percent, Some(25.0));
        assert!(!m.kv_cache_is_estimated);
        // The labelled `requests_total` must NOT be read as the total: the
        // parser keys by name, so it would report one outcome's count.
        assert_eq!(m.total_requests, Some(10));
        assert_eq!(m.total_prompt_tokens, Some(1000));
        assert_eq!(m.total_generation_tokens, Some(500));
        assert_eq!(m.prefix_cache_hit_rate, Some(40.0));
        assert_eq!(m.preemptions_total, Some(3));
        assert_eq!(m.avg_batch_size, Some(2.5));
    }

    /// Latency means come from histogram sum/count; the decode span is duration
    /// minus first-token time, and ITL divides it by the decode tokens covered.
    #[test]
    fn derives_decode_metrics_from_histograms() {
        let m = mapped(
            &[
                ("generation_tokens_total", 500.0),
                ("decode_tokens_total", 490.0),
            ],
            &[],
        );

        assert_eq!(m.ttft_ms, Some(2000.0));
        assert_eq!(m.e2e_latency_ms, Some(4000.0));
        assert_eq!(m.queue_time_ms, Some(50.0));
        // 40 s of requests − 20 s to first token = 20 s decoding 490 tokens.
        assert_eq!(m.inter_token_latency_ms, Some((20.0 / 490.0) * 1000.0));
        assert_eq!(m.per_request_tps, Some(25.0));
        // No TPOT histogram exists, so nothing feeds that family.
        assert!(m.tpot_ms.is_none() && m.tpot_percentiles.is_none());
    }

    /// A window holding a request's first token but not its completion has no
    /// decode span; claiming one would publish a negative duration.
    #[test]
    fn no_decode_span_when_duration_lags_ttft() {
        let m = mapped_with(
            &[("decode_tokens_total", 10.0)],
            &[],
            (10.0, 60.0),
            (10.0, 40.0),
            (10.0, 0.5),
        );
        assert!(m.inter_token_latency_ms.is_none());
        assert!(m.per_request_tps.is_none());
    }

    /// Percentiles and goodput interpolate from cumulative buckets, and the raw
    /// buckets ride to the frontend for custom SLOs. `+Inf` must arrive as the
    /// finite `f64::MAX` sentinel, since serde_json cannot encode infinity.
    #[test]
    fn percentiles_goodput_and_buckets_survive_the_wire_format() {
        let m = mapped(&[], &[]);

        let p = m.ttft_percentiles.expect("percentiles");
        assert!(p.p50_ms.is_some() && p.p95_ms.is_some() && p.p99_ms.is_some());
        let buckets = m.ttft_buckets.expect("buckets");
        assert!(buckets.iter().any(|b| b.le_seconds == f64::MAX));
        assert!(buckets.iter().all(|b| b.le_seconds.is_finite()));
        assert!(m.ttft_goodput_pct.is_some() && m.e2e_goodput_pct.is_some());
        assert!(m.e2e_percentiles.is_some() && m.e2e_buckets.is_some());
    }

    /// Infernix counts verification rounds, not draft attempts, so rounds are
    /// what `drafts` means for this engine.
    #[test]
    fn spec_decode_rounds_are_the_draft_counter() {
        let m = mapped(
            &[
                ("spec_decode_rounds_total", 50.0),
                ("spec_decode_draft_tokens_total", 200.0),
                ("spec_decode_accepted_tokens_total", 120.0),
            ],
            &[],
        );

        assert_eq!(m.spec_decode_drafts_total, Some(50));
        assert_eq!(m.spec_decode_draft_tokens_total, Some(200));
        assert_eq!(m.spec_decode_accepted_tokens_total, Some(120));
        assert_eq!(m.spec_decode_acceptance_rate, Some(60.0));
        assert_eq!(m.spec_decode_mean_acceptance_length, Some(2.4));
    }

    /// With no speculative backend the counters are absent, and the frontend
    /// uses their absence to hide the section entirely.
    #[test]
    fn absent_spec_decode_stays_absent() {
        let m = mapped_with(&[], &[], (0.0, 0.0), (0.0, 0.0), (0.0, 0.0));
        assert!(m.spec_decode_drafts_total.is_none());
        assert!(m.spec_decode_acceptance_rate.is_none());
        assert!(m.ttft_ms.is_none(), "count 0 must not divide");
    }

    /// Warmup withholds every derived value — including the ones `map_metrics`
    /// owns — while the live gauges keep reporting.
    #[test]
    fn warmup_blanks_derived_values_but_keeps_live_state() {
        let text = body(
            &[("generation_tokens_total", 500.0)],
            &[
                ("requests_running", 3.0),
                ("device_kv_used_pages", 10.0),
                ("device_kv_capacity_pages", 20.0),
            ],
            (10.0, 20.0),
            (40.0, 10.0),
            (0.5, 10.0),
        );
        let parsed = parse_prometheus_text(&text).expect("parse");
        let m = map_metrics(&parsed, &parsed, true);

        assert!(m.ttft_ms.is_none() && m.e2e_latency_ms.is_none());
        assert!(m.inter_token_latency_ms.is_none() && m.avg_batch_size.is_none());
        assert!(m.ttft_percentiles.is_none() && m.ttft_buckets.is_none());
        assert_eq!(m.active_requests, Some(3));
        assert_eq!(m.kv_cache_percent, Some(50.0));
        assert_eq!(m.total_requests, Some(10));
    }

    /// End-to-end through the HTTP surface: `/metrics` is gated by `--api-key`,
    /// so the adapter has to carry the bearer token, and the first poll reports
    /// warmup rather than derived latencies.
    #[tokio::test]
    async fn polls_gated_metrics_and_reports_warmup_first() {
        use axum::routing::get;
        use axum::{http::HeaderMap, http::StatusCode, Router};

        const KEY: &str = "test-key";
        async fn metrics(headers: HeaderMap) -> Result<String, StatusCode> {
            let authorized = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Bearer {KEY}"));
            if !authorized {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(body(
                &[("generation_tokens_total", 500.0)],
                &[("requests_running", 1.0)],
                (10.0, 20.0),
                (40.0, 10.0),
                (0.5, 10.0),
            ))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/metrics", get(metrics)))
                .await
                .unwrap();
        });

        let adapter = InfernixAdapter::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            None,
            Some(KEY.to_string()),
        );

        // First poll: warming, so no derived latency — but the gauge still
        // shows live engine state.
        let first = adapter.get_metrics().await.expect("metrics");
        assert!(first.warming_up);
        assert!(first.ttft_ms.is_none());
        assert_eq!(first.active_requests, Some(1));

        // Without the key the gated endpoint answers 401; the body is not
        // metrics, so nothing is reported rather than a page of confident zeros.
        let unauthenticated =
            InfernixAdapter::new(reqwest::Client::new(), format!("http://{addr}"), None, None);
        let leaked = unauthenticated
            .get_metrics()
            .await
            .map(|m| (m.active_requests, m.total_requests));
        assert!(
            leaked.is_none() || leaked == Some((None, None)),
            "401 body must not yield values: {leaked:?}"
        );
    }
}
