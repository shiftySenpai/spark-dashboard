//! Per-request rows scraped from an engine's own log output.
//!
//! Strata reports finished requests natively (`GET /metrics` `requests[]`),
//! but vLLM and llama.cpp expose no per-request API at all — their `/metrics`
//! is aggregate-only (verified: vLLM emits 106 `vllm:*` names, llama.cpp
//! `llamacpp_*` counters). Their only per-request trace is their own log:
//!
//! * llama.cpp prints a three-line `slot print_timing` block per completed
//!   request (prompt eval / eval / total), which carries exact token counts
//!   and timings.
//! * vLLM's OpenAI server answers through uvicorn, whose access log prints
//!   one line per request (`INFO: ip:port - "POST /v1/... HTTP/1.1" 200 OK`)
//!   unless `--disable-uvicorn-access-log` was passed. That line carries no
//!   token counts, so those rows are sparse by construction.
//!
//! Rows are deltas: `poll()` returns only what arrived since the previous
//! call, matching the `EngineSnapshot::recent_requests` wire semantics.
//! Start/end times are the line's arrival time at the tail (the log stream
//! is follow-mode, so that is within a poll interval of the request
//! finishing); `source: "log"` marks the reduced provenance.

use super::{EngineType, RecentRequest};
use bollard::query_parameters::LogsOptionsBuilder;
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

/// A live per-engine log tail, parsed into request rows.
pub struct RequestLog {
    engine_type: EngineType,
    inner: Inner,
    /// llama.cpp's three timing lines accumulate per (slot id, task id)
    /// until the "total time" line completes the request.
    partial: HashMap<(u32, u32), PartialTiming>,
    /// Trailing bytes of the last read that had no newline yet (file source).
    partial_line: String,
}

enum Inner {
    /// Docker follow stream, fed by a background task. When the task exits
    /// (container stopped, daemon hiccup) the channel closes and `poll()`
    /// simply yields nothing; `ensure` rebuilds the tail if the container id
    /// changes.
    Container {
        container_id: String,
        rx: mpsc::UnboundedReceiver<String>,
    },
    /// A native engine's own log file (`llama-server -f <path>`), read from
    /// `offset` on every poll tick.
    File {
        path: PathBuf,
        file: File,
        offset: u64,
    },
}

#[derive(Default)]
struct PartialTiming {
    prompt_ms: Option<f64>,
    prompt_tokens: Option<u64>,
    decode_ms: Option<f64>,
    output_tokens: Option<u64>,
}

/// (Re)create the tail when the engine has a log source and none is attached,
/// or when the container id changed (engine restarted in a new container).
/// Native engines without a `-f`/`--log-file` flag have no readable log and
/// stay without a tail — honestly no rows, rather than a guess.
pub fn ensure(
    slot: &mut Option<RequestLog>,
    engine_type: &EngineType,
    container_id: Option<&str>,
    log_file: Option<&str>,
) {
    // Strata reports requests natively; a log tail would only duplicate them.
    if matches!(engine_type, EngineType::Strata) {
        return;
    }
    let source_key = match (container_id, log_file) {
        (Some(id), _) => Some(("container", id.to_string())),
        (None, Some(path)) => Some(("file", path.to_string())),
        (None, None) => None,
    };
    let Some((kind, key)) = source_key else {
        return;
    };

    let attached = slot
        .as_ref()
        .is_some_and(|log| (log_source_kind(log), log_source_key(log)) == (kind, key.as_str()));
    if attached {
        return;
    }

    *slot = match kind {
        "container" => {
            let (tx, rx) = mpsc::unbounded_channel();
            spawn_container_task(key.clone(), tx);
            Some(RequestLog {
                engine_type: engine_type.clone(),
                inner: Inner::Container {
                    container_id: key,
                    rx,
                },
                partial: HashMap::new(),
                partial_line: String::new(),
            })
        }
        _ => {
            // Start at the end of an existing log: replaying a whole engine
            // log on adoption would flood the panel with ancient requests.
            match std::fs::File::open(&key) {
                Ok(f) => {
                    let offset = f.metadata().map(|m| m.len()).unwrap_or(0);
                    Some(RequestLog {
                        engine_type: engine_type.clone(),
                        inner: Inner::File {
                            path: PathBuf::from(&key),
                            file: File::from(f),
                            offset,
                        },
                        partial: HashMap::new(),
                        partial_line: String::new(),
                    })
                }
                Err(_) => None,
            }
        }
    };
}

fn log_source_kind(log: &RequestLog) -> &'static str {
    match &log.inner {
        Inner::Container { .. } => "container",
        Inner::File { .. } => "file",
    }
}

fn log_source_key(log: &RequestLog) -> &str {
    match &log.inner {
        Inner::Container { container_id, .. } => container_id,
        Inner::File { path, .. } => path.to_str().unwrap_or(""),
    }
}

fn spawn_container_task(container_id: String, tx: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let Ok(docker) = Docker::connect_with_local_defaults() else {
            return;
        };
        // tail("0"): only lines produced from now on. Replaying old log
        // lines would stamp ancient requests with arrival-time timestamps
        // and show them as recent — the gap between detection and adoption
        // is at most a few seconds, not worth that lie.
        let options = LogsOptionsBuilder::new()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .tail("0")
            .build();
        let mut stream = docker.logs(&container_id, Some(options));
        let mut buf = String::new();
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(bollard::container::LogOutput::StdOut { message }) => message,
                Ok(bollard::container::LogOutput::StdErr { message }) => message,
                Ok(bollard::container::LogOutput::Console { message }) => message,
                _ => continue,
            };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                if tx.send(line).is_err() {
                    return;
                }
            }
        }
    });
}

impl RequestLog {
    /// Drain everything that arrived since the last call and parse it.
    pub async fn poll(&mut self) -> Vec<RecentRequest> {
        let now_ms = epoch_ms();
        let mut lines: Vec<String> = Vec::new();
        match &mut self.inner {
            Inner::Container { rx, .. } => {
                while let Ok(line) = rx.try_recv() {
                    lines.push(line);
                }
            }
            Inner::File { file, offset, .. } => {
                // The log may have been rotated/truncated under us: a file
                // shorter than the cursor means start over.
                let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
                if len < *offset {
                    *offset = 0;
                    self.partial_line.clear();
                }
                // One tick's worth at most; a busier engine than this simply
                // has its backlog read over the next few ticks.
                // ponytail: sequential catch-up, cap higher if bursts matter
                const MAX_TICK_BYTES: u64 = 256 * 1024;
                let to_read = (len - *offset).min(MAX_TICK_BYTES);
                if to_read > 0 && file.seek(std::io::SeekFrom::Start(*offset)).await.is_ok() {
                    let mut chunk = vec![0u8; to_read as usize];
                    if file.read_exact(&mut chunk).await.is_ok() {
                        *offset += to_read;
                        self.partial_line.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = self.partial_line.find('\n') {
                            let line: String =
                                self.partial_line[..pos].trim_end_matches('\r').to_string();
                            self.partial_line.drain(..=pos);
                            lines.push(line);
                        }
                    }
                }
            }
        }
        let mut rows = Vec::new();
        for line in &lines {
            if let Some(row) = self.parse_line(line, now_ms) {
                rows.push(row);
            }
        }
        // A slot/task whose "total time" line never arrives (crash, log
        // rotation mid-block) must not grow the map forever.
        if self.partial.len() > 512 {
            self.partial.clear();
        }
        rows
    }

    fn parse_line(&mut self, line: &str, now_ms: u64) -> Option<RecentRequest> {
        match self.engine_type {
            EngineType::LlamaCpp => self.parse_llama_line(line, now_ms),
            EngineType::Vllm => parse_vllm_line(line, now_ms),
            EngineType::Strata => None,
        }
    }

    /// llama.cpp per-request block, one line each:
    /// `slot print_timing: id  0 | task 25 | prompt eval time = 1000.00 ms / 200 tokens (…)`
    /// `slot print_timing: id  0 | task 25 | eval time = 5000.00 ms / 150 tokens (…)`
    /// `slot print_timing: id  0 | task 25 | total time = 6000.00 ms / 350 tokens`
    /// Lines from different slots interleave, hence the (id, task) map.
    fn parse_llama_line(&mut self, line: &str, now_ms: u64) -> Option<RecentRequest> {
        let at = line.find("slot print_timing: id")?;
        let rest = &line[at + "slot print_timing: id".len()..];
        let (id, rest) = parse_usize(rest)?;
        let task_at = rest.find("task ")?;
        let (task, rest) = parse_usize(&rest[task_at + "task ".len()..])?;
        let key = (id as u32, task as u32);

        if let Some(after) = rest
            .find("prompt eval time = ")
            .map(|i| &rest[i + "prompt eval time = ".len()..])
        {
            let (ms, after) = parse_f64(after)?;
            let (tokens, _) = parse_usize(skip_until(after, " / "))?;
            let p = self.partial.entry(key).or_default();
            p.prompt_ms = Some(ms);
            p.prompt_tokens = Some(tokens as u64);
            return None;
        }
        if let Some(after) = rest
            .find(" | eval time = ")
            .map(|i| &rest[i + " | eval time = ".len()..])
        {
            let (ms, after) = parse_f64(after)?;
            let (tokens, _) = parse_usize(skip_until(after, " / "))?;
            let p = self.partial.entry(key).or_default();
            p.decode_ms = Some(ms);
            p.output_tokens = Some(tokens as u64);
            return None;
        }
        let after = rest
            .find("total time = ")
            .map(|i| &rest[i + "total time = ".len()..])?;
        let (total_ms, _) = parse_f64(after)?;
        let p = self.partial.remove(&key).unwrap_or_default();
        let tokens_per_sec = match (p.decode_ms, p.output_tokens) {
            (Some(ms), Some(n)) if ms > 0.0 => Some(n as f64 / (ms / 1000.0)),
            _ => None,
        };
        Some(RecentRequest {
            start_ms: now_ms.saturating_sub(total_ms as u64),
            end_ms: now_ms,
            tokens_per_sec,
            ttft_ms: p.prompt_ms,
            prompt_tokens: p.prompt_tokens,
            output_tokens: p.output_tokens,
            finish: None,
            prefix_cache_hit_rate: None,
            source: "log".to_string(),
        })
    }
}

/// vLLM uvicorn access line: `INFO: 127.0.0.1:34658 - "POST /v1/chat/completions HTTP/1.1" 200 OK`.
/// No token counts are in it, so the row is sparse: one completed inference
/// request, placed in time. Only 2xx; GET endpoints (/metrics polls,
/// /v1/models) never match the POST filter.
fn parse_vllm_line(line: &str, now_ms: u64) -> Option<RecentRequest> {
    let at = line.find("\"POST /v1/")?;
    let after = &line[at + "\"POST /v1/".len()..];
    let end = after.find(" HTTP/1.1\"")?;
    let status_at = end + " HTTP/1.1\"".len();
    let status: u16 = after[status_at..]
        .trim_start()
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    Some(RecentRequest {
        start_ms: now_ms,
        end_ms: now_ms,
        tokens_per_sec: None,
        ttft_ms: None,
        prompt_tokens: None,
        output_tokens: None,
        finish: None,
        prefix_cache_hit_rate: None,
        source: "log".to_string(),
    })
}

/// Parse a leading integer (after optional spaces), returning it and the rest.
fn parse_usize(s: &str) -> Option<(usize, &str)> {
    let s = s.trim_start();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits == 0 {
        return None;
    }
    Some((s[..digits].parse().ok()?, &s[digits..]))
}

/// Parse a leading float, returning it and the rest.
fn parse_f64(s: &str) -> Option<(f64, &str)> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].parse().ok()?, &s[end..]))
}

fn skip_until<'a>(s: &'a str, needle: &str) -> &'a str {
    s.find(needle).map(|i| &s[i + needle.len()..]).unwrap_or(s)
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_log(name: &str, lines: &[&str]) -> RequestLog {
        let dir = std::env::temp_dir().join(format!("spark_reqlog_{name}_{}", epoch_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.log");
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        // The tail attaches at EOF in production; tests replay from 0.
        RequestLog {
            engine_type: EngineType::LlamaCpp,
            inner: Inner::File {
                path: path.clone(),
                file: std::fs::File::open(&path).unwrap().into(),
                offset: 0,
            },
            partial: HashMap::new(),
            partial_line: String::new(),
        }
    }

    #[test]
    fn llama_print_timing_block_yields_one_exact_row() {
        let mut log = file_log(
            "exact",
            &[
                "2026-10-06T12:00:00.000Z I slot get_avail_slot: id  0 | task 25 | picked",
                "2026-10-06T12:00:06.000Z I slot print_timing: id  0 | task 25 | prompt eval time = 1000.00 ms / 200 tokens (5.00 ms per token, 200.00 tokens per second)",
                "2026-10-06T12:00:11.000Z I slot print_timing: id  0 | task 25 | eval time = 5000.00 ms / 150 tokens (33.33 ms per token, 30.00 tokens per second)",
                "2026-10-06T12:00:11.000Z I slot print_timing: id  0 | task 25 | total time = 6000.00 ms / 350 tokens",
            ],
        );
        let rows = futures_block(log.poll());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.prompt_tokens, Some(200));
        assert_eq!(r.output_tokens, Some(150));
        assert_eq!(r.ttft_ms, Some(1000.0));
        assert_eq!(r.source, "log");
        // end - start = total time; tps = 150 / 5s.
        assert_eq!(r.end_ms - r.start_ms, 6000);
        assert!((r.tokens_per_sec.unwrap() - 30.0).abs() < 0.01);
    }

    #[test]
    fn llama_interleaved_slots_do_not_mix() {
        let mut log = file_log(
            "interleaved",
            &[
                "slot print_timing: id  0 | task 1 | prompt eval time = 100.00 ms / 10 tokens (…)",
                "slot print_timing: id  1 | task 2 | prompt eval time = 200.00 ms / 20 tokens (…)",
                "slot print_timing: id  0 | task 1 | eval time = 300.00 ms / 30 tokens (…)",
                "slot print_timing: id  1 | task 2 | eval time = 400.00 ms / 40 tokens (…)",
                "slot print_timing: id  0 | task 1 | total time = 400.00 ms / 40 tokens",
                "slot print_timing: id  1 | task 2 | total time = 600.00 ms / 60 tokens",
            ],
        );
        let rows = futures_block(log.poll());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].output_tokens, Some(30));
        assert_eq!(rows[1].output_tokens, Some(40));
        assert_eq!(rows[1].ttft_ms, Some(200.0));
    }

    #[test]
    fn vllm_access_lines_are_sparse_2xx_rows() {
        let mut log = file_log(
            "vllm",
            &[
                "INFO: 127.0.0.1:34658 - \"POST /v1/chat/completions HTTP/1.1\" 200 OK",
                "INFO: 127.0.0.1:34659 - \"GET /metrics HTTP/1.1\" 200 OK",
                "INFO: 127.0.0.1:34660 - \"POST /v1/completions HTTP/1.1\" 404 Not Found",
                "INFO 10-06 12:00:00 [loggers.py:123] Engine 000: Avg prompt throughput: 0.0 tokens/s",
            ],
        );
        log.engine_type = EngineType::Vllm;
        let rows = futures_block(log.poll());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens_per_sec, None);
        assert_eq!(rows[0].start_ms, rows[0].end_ms);
        assert_eq!(rows[0].source, "log");
    }

    #[test]
    fn poll_is_a_delta() {
        let mut log = file_log(
            "delta",
            &["slot print_timing: id  0 | task 1 | total time = 5.00 ms / 5 tokens"],
        );
        assert_eq!(futures_block(log.poll()).len(), 1);
        assert!(futures_block(log.poll()).is_empty());
    }

    /// Tiny helper: `poll` is async but the file source never blocks, so a
    /// minimal block_on is enough for tests (no tokio macro dependency here).
    fn futures_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }
}
