//! Strata inference engine adapter.
//!
//! Strata's front (`serve/server.py`) exposes a purpose-built JSON `/metrics`
//! — lifetime totals, live per-job rates, and speculative-decode counters —
//! alongside llama.cpp-style `/health` and an OpenAI-style `/v1/models`.
//! Unlike vLLM and llama.cpp there is nothing to diff or derive here: every
//! field the dashboard shows is already computed engine-side, so this adapter
//! is a mapping, not a derivation.
//!
//! Two engine-side facts carried in the mapping:
//! * `totals` settle at request completion (the front settles a request's
//!   history and totals on the fifo), so the in-flight task's
//!   `live.generated` / `live.prompt_read` are added to the lifetime totals —
//!   the same live-token treatment the llama.cpp adapter gives via `/slots`
//!   deltas. `live.state` is read before the addition: an idle `live` block
//!   carries the last job's values, which are already inside `totals`.
//! * `live.state` is one engine-wide state string ("idle", "reading",
//!   "generating", …), so `active_requests` is 0/1 — Strata generates one
//!   job at a time and reports the backlog in `live.queued`.

use super::{
    EngineAdapter, EngineMetrics, EngineStatus, EngineType, ModelInfo, ModelMetadataError,
    ModelResolution,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

pub struct StrataAdapter {
    client: reqwest::Client,
    endpoint: String,
    /// Model identity recovered from the launch command line. Strata's
    /// `/v1/models` answers openly, so this is only ever a fallback.
    served_model: Option<String>,
}

impl StrataAdapter {
    pub fn new(client: reqwest::Client, endpoint: String, served_model: Option<String>) -> Self {
        Self {
            client,
            endpoint,
            served_model,
        }
    }
}

// ---------------------------------------------------------------------------
// /metrics reply shape (trimmed to the fields the dashboard reads)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StrataMetricsReply {
    totals: StrataTotals,
    live: StrataLive,
}

/// Lifetime totals, settled per request at completion.
#[derive(Deserialize)]
struct StrataTotals {
    requests: u64,
    prompt_tokens: u64,
    /// Prompt tokens served from the prefix/prompt cache.
    reused: u64,
    output_tokens: u64,
    prompt_ms: f64,
    decode_ms: f64,
    drafts_offered: u64,
    drafts_accepted: u64,
}

/// The in-flight job (or the last one, while idle).
#[derive(Deserialize)]
struct StrataLive {
    state: String,
    #[serde(default)]
    queued: u64,
    #[serde(default)]
    generated: i64,
    #[serde(default)]
    prompt_read: Option<i64>,
    #[serde(default)]
    tok_s: Option<f64>,
    #[serde(default)]
    tok_s_mean: Option<f64>,
    #[serde(default)]
    prefill_tok_s_mean: Option<f64>,
}

/// Map the JSON reply onto the shared `EngineMetrics` shape. Fields Strata
/// has no source for (KV usage, histograms, queue time, preemptions, batch
/// size) stay `None` so the UI hides them.
fn map_metrics(reply: StrataMetricsReply) -> EngineMetrics {
    let t = &reply.totals;
    let l = &reply.live;
    let busy = l.state != "idle";

    // In-flight progress on top of the completion-settled totals; ignored
    // while idle, where `live` repeats the last job already counted.
    let inflight_generated = if busy { l.generated.max(0) } else { 0 };
    let inflight_prompt = if busy {
        l.prompt_read.unwrap_or(0).max(0)
    } else {
        0
    };

    let per_request_tps =
        (t.decode_ms > 0.0).then(|| t.output_tokens as f64 / (t.decode_ms / 1000.0));
    let per_request_prompt_tps =
        (t.prompt_ms > 0.0).then(|| t.prompt_tokens as f64 / (t.prompt_ms / 1000.0));
    let tpot_ms = (t.output_tokens > 0).then(|| t.decode_ms / t.output_tokens as f64);
    let ttft_ms = (t.requests > 0).then(|| t.prompt_ms / t.requests as f64);
    let e2e_latency_ms = (t.requests > 0).then(|| (t.prompt_ms + t.decode_ms) / t.requests as f64);
    let prefix_cache_hit_rate =
        (t.prompt_tokens > 0).then(|| (t.reused as f64 / t.prompt_tokens as f64) * 100.0);
    let spec_decode_acceptance_rate = (t.drafts_offered > 0)
        .then(|| (t.drafts_accepted as f64 / t.drafts_offered as f64) * 100.0);

    EngineMetrics {
        // Live rates come straight from the engine's own per-job measurement;
        // `tok_s` is null while prefilling, which reads as a dash — honest,
        // since no decode tokens have landed yet.
        tokens_per_sec: l.tok_s,
        avg_tokens_per_sec: l.tok_s_mean,
        per_request_tps,
        ttft_ms,
        active_requests: Some(u64::from(busy)),
        queued_requests: Some(l.queued),
        kv_cache_percent: None,
        kv_cache_is_estimated: false,
        total_requests: Some(t.requests),
        e2e_latency_ms,
        // Strata reports only the mean prefill rate, not an instantaneous
        // one, so both the live and average prompt-rate fields carry it.
        prompt_tokens_per_sec: l.prefill_tok_s_mean.filter(|v| *v > 0.0),
        avg_prompt_tokens_per_sec: l.prefill_tok_s_mean.filter(|v| *v > 0.0),
        per_request_prompt_tps,
        swapped_requests: None,
        prefix_cache_hit_rate,
        queue_time_ms: None,
        inter_token_latency_ms: tpot_ms,
        preemptions_total: None,
        total_prompt_tokens: Some((t.prompt_tokens as i64 + inflight_prompt).max(0) as u64),
        total_generation_tokens: Some((t.output_tokens as i64 + inflight_generated).max(0) as u64),
        prefix_cache_queries_total: Some(t.prompt_tokens),
        avg_batch_size: None,
        // No histograms engine-side yet — distribution fields stay hidden.
        ttft_percentiles: None,
        itl_percentiles: None,
        e2e_percentiles: None,
        ttft_goodput_pct: None,
        itl_goodput_pct: None,
        e2e_goodput_pct: None,
        ttft_buckets: None,
        itl_buckets: None,
        e2e_buckets: None,
        tpot_ms,
        tpot_percentiles: None,
        tpot_goodput_pct: None,
        tpot_buckets: None,
        spec_decode_draft_tokens_total: Some(t.drafts_offered),
        spec_decode_accepted_tokens_total: Some(t.drafts_accepted),
        // Strata counts offered/accepted draft tokens, not draft attempts —
        // acceptance length has no denominator.
        spec_decode_drafts_total: None,
        spec_decode_acceptance_rate,
        spec_decode_acceptance_rate_live: None,
        spec_decode_mean_acceptance_length: None,
        warming_up: t.requests < 1,
    }
}

#[async_trait]
impl EngineAdapter for StrataAdapter {
    fn engine_type(&self) -> EngineType {
        EngineType::Strata
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn health_check(&self) -> EngineStatus {
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
        // OpenAI-shaped `{"data":[{"id":…}]}`, like vLLM's. Strata serves it
        // open (no auth gate in the front), so a non-success is a genuine
        // unavailability; the command-line hint is only a fallback.
        let reply = match self
            .client
            .get(format!("{}/v1/models", self.endpoint))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<OpenAIModelsReply>().await {
                    Ok(m) => m.data.first().map(|m| m.id.clone()),
                    Err(_) => None,
                }
            }
            Ok(resp) => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    status = %resp.status(),
                    "/v1/models returned non-success",
                );
                return ModelResolution {
                    model: self.served_model.as_deref().map(display_model),
                    metadata_error: Some(ModelMetadataError::Unavailable),
                };
            }
            Err(e) => {
                tracing::debug!(endpoint = %self.endpoint, error = %e, "/v1/models request failed");
                return ModelResolution {
                    model: self.served_model.as_deref().map(display_model),
                    metadata_error: Some(ModelMetadataError::Unavailable),
                };
            }
        };

        match reply.or_else(|| self.served_model.clone()) {
            Some(name) => ModelResolution {
                model: Some(display_model(&name)),
                metadata_error: None,
            },
            None => ModelResolution {
                model: None,
                metadata_error: Some(ModelMetadataError::Unavailable),
            },
        }
    }

    async fn get_metrics(&self) -> Option<EngineMetrics> {
        let resp = self
            .client
            .get(format!("{}/metrics", self.endpoint))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let reply: StrataMetricsReply = resp.json().await.ok()?;
        Some(map_metrics(reply))
    }
}

/// `/v1/models` reply shape (OpenAI list, same as vLLM's).
#[derive(Deserialize)]
struct OpenAIModelsReply {
    #[serde(default)]
    data: Vec<OpenAIModel>,
}

#[derive(Deserialize)]
struct OpenAIModel {
    id: String,
}

/// Reduce a model id to a display `ModelInfo`. Strata's id is already a short
/// name (`qwen3.8-flash-next-iq3_s`); a command-line hint may be a path, so
/// the basename rule from the llama.cpp adapter applies too. Params/quant
/// enrichment is N/A — no HF repo for a local pack.
fn display_model(name: &str) -> ModelInfo {
    let display = name
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name)
        .to_string();
    ModelInfo {
        name: display,
        parameter_size: None,
        quantization: None,
        precision: None,
        tensor_type: None,
        model_type: None,
        pipeline_tag: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/metrics` reply in the shape captured from the live front
    /// (mid-prefill: `state=reading`, `tok_s=null`).
    fn reply(json: &str) -> StrataMetricsReply {
        serde_json::from_str(json).expect("parse")
    }

    const BASE: &str = r#"{
        "totals": {"since": 1791290241.78, "requests": 366, "prompt_tokens": 40581991,
                   "reused": 31125942, "output_tokens": 219360, "prompt_ms": 1600630.0,
                   "decode_ms": 1266767.0, "drafts_offered": 188641, "drafts_accepted": 135115},
        "live": {"state": "generating", "queued": 1, "generated": 42, "prompt_read": null,
                 "tok_s": 160.7, "tok_s_mean": 158.2, "prefill_tok_s_mean": 0.0}
    }"#;

    #[test]
    fn maps_totals_rates_and_spec() {
        let m = map_metrics(reply(BASE));
        assert!(!m.warming_up);
        assert_eq!(m.total_requests, Some(366));
        // 219360 settled + 42 in-flight generated tokens.
        assert_eq!(m.total_generation_tokens, Some(219402));
        assert_eq!(m.total_prompt_tokens, Some(40581991));
        assert_eq!(m.active_requests, Some(1));
        assert_eq!(m.queued_requests, Some(1));
        assert_eq!(m.tokens_per_sec, Some(160.7));
        assert_eq!(m.avg_tokens_per_sec, Some(158.2));
        // prefill_tok_s_mean = 0 → blanked, not a confident zero.
        assert_eq!(m.prompt_tokens_per_sec, None);
        let close = |a: Option<f64>, b: f64| a.is_some_and(|v| (v - b).abs() < 0.01);
        assert!(close(m.tpot_ms, 1266767.0 / 219360.0));
        assert!(close(m.inter_token_latency_ms, 1266767.0 / 219360.0));
        assert!(close(m.ttft_ms, 1600630.0 / 366.0));
        assert!(close(m.e2e_latency_ms, (1600630.0 + 1266767.0) / 366.0));
        assert!(close(m.per_request_tps, 219360.0 / (1266767.0 / 1000.0)));
        assert!(close(
            m.prefix_cache_hit_rate,
            31125942.0 / 40581991.0 * 100.0
        ));
        assert!(close(
            m.spec_decode_acceptance_rate,
            135115.0 / 188641.0 * 100.0
        ));
        assert_eq!(m.spec_decode_draft_tokens_total, Some(188641));
        assert_eq!(m.spec_decode_accepted_tokens_total, Some(135115));
        assert_eq!(m.spec_decode_drafts_total, None);
        // No histogram/KV/queue sources — hidden, not zeroed.
        assert_eq!(m.kv_cache_percent, None);
        assert!(m.ttft_percentiles.is_none());
        assert_eq!(m.queue_time_ms, None);
        assert_eq!(m.preemptions_total, None);
    }

    #[test]
    fn idle_live_block_does_not_double_count_totals() {
        // Idle `live` carries the last job's values — already inside `totals`.
        let m = map_metrics(reply(
            r#"{"totals": {"requests": 2, "prompt_tokens": 100, "reused": 0,
                           "output_tokens": 50, "prompt_ms": 10.0, "decode_ms": 20.0,
                           "drafts_offered": 0, "drafts_accepted": 0},
                 "live": {"state": "idle", "queued": 0, "generated": 50,
                          "prompt_read": 100, "tok_s": null, "tok_s_mean": null,
                          "prefill_tok_s_mean": null}}"#,
        ));
        assert_eq!(m.total_generation_tokens, Some(50));
        assert_eq!(m.total_prompt_tokens, Some(100));
        assert_eq!(m.active_requests, Some(0));
    }

    #[test]
    fn prefill_progress_adds_prompt_tokens_live() {
        let m = map_metrics(reply(
            r#"{"totals": {"requests": 1, "prompt_tokens": 100, "reused": 0,
                           "output_tokens": 10, "prompt_ms": 10.0, "decode_ms": 5.0,
                           "drafts_offered": 0, "drafts_accepted": 0},
                 "live": {"state": "reading", "queued": 0, "generated": 0,
                          "prompt_read": 4000, "tok_s": null, "tok_s_mean": null,
                          "prefill_tok_s_mean": 2100.0}}"#,
        ));
        assert_eq!(m.total_prompt_tokens, Some(4100));
        assert_eq!(m.total_generation_tokens, Some(10));
        assert_eq!(m.tokens_per_sec, None);
        assert_eq!(m.prompt_tokens_per_sec, Some(2100.0));
    }

    #[test]
    fn no_requests_yet_is_warming() {
        let m = map_metrics(reply(
            r#"{"totals": {"requests": 0, "prompt_tokens": 0, "reused": 0,
                           "output_tokens": 0, "prompt_ms": 0.0, "decode_ms": 0.0,
                           "drafts_offered": 0, "drafts_accepted": 0},
                 "live": {"state": "idle", "queued": 0, "generated": 0,
                          "prompt_read": null, "tok_s": null, "tok_s_mean": null,
                          "prefill_tok_s_mean": null}}"#,
        ));
        assert!(m.warming_up);
        assert_eq!(m.ttft_ms, None);
        assert_eq!(m.per_request_tps, None);
        assert_eq!(m.total_requests, Some(0));
    }

    #[test]
    fn display_model_reduces_paths_to_basename() {
        assert_eq!(
            display_model("qwen3.8-flash-next-iq3_s").name,
            "qwen3.8-flash-next-iq3_s"
        );
        assert_eq!(display_model("/x/packs/iq3_s").name, "iq3_s");
    }
}
