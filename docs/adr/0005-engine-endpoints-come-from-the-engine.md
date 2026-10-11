# ADR 0005: Engine endpoints come from the engine, not from a table

- **Status:** Proposed
- **Date:** 2026-10-11
- **Decided by:** field report — Strata vanished from the dashboard after an engine restart

## Context

`ENGINE_BINARIES` (`src/engines/detector.rs:35-48`) pairs each engine binary with a
default endpoint: vLLM `http://localhost:8000`, llama-server `http://localhost:8080`,
`strata` `http://localhost:8095`, `infernix-serve` `http://localhost:8080`. That
default is what a candidate is reached at when its command line carries no
`--port` / `--host` (`parse_endpoint_from_args`, `src/engines/detector.rs:351-392`).

Facts that constrain the design, all verified on this host (Linux, 2026-10-11):

- **Operators change the port.** Strata's own default is 8095 (`serve/server.py:6026`),
  and its installer starts it on 8080. The table's guess is a port nobody chose.
- **Strata is two processes.** The native engine (`engine/strata --serve …`) carries
  no port at all; the Python front (`serve/server.py … --port 8080`) carries it and
  owns the socket. Detection unions both scans and expands the PID tree
  (`src/engines/detector.rs:209-231`, `:280-308`) — so the engine contributes PIDs
  for GPU attribution and the front contributes the endpoint. Whichever process the
  scan loses decides whether the engine appears.
- **argv is the only cross-user channel.** The dashboard runs as user
  `spark-dashboard`; engines run as whoever installed them. On Linux
  `/proc/<pid>/cmdline` is `0444`, but `/proc/<pid>/environ` is `0400` and
  `/proc/<pid>/fd` is `0500`. So the two obvious ways to learn a port — "which
  socket does this PID own" (`/proc/net/tcp` inode → fd) and "what did its
  environment say" — are unavailable to an unrooted scanner.
- **An engine that disappears stays disappeared.** `record_probe_result` and
  `should_remove` (`src/engines/mod.rs:443-470`) mark an engine Stopped after 3
  consecutive poll failures and drop it from the active list 30 s later
  (`src/engines/mod.rs:795`). On this host the service logged
  `Detected engine: Strata at http://localhost:8080` once, at 09:51:58, and after
  the engine was restarted it carried `engines: []` on every tick — it was never
  re-added, and nothing logged the removal.
- **The port is not the identity.** The engine type comes from the binary name and
  the argv match (`cmdline_matches_engine`, `:150-166`); the model name comes from
  `/v1/models` with the argv hint as fallback. Nothing about identification needs
  a port. The endpoint only decides whether the engine's own numbers are readable.

## Decision

**1. The endpoint is read from the process; it is never guessed.**
`ENGINE_BINARIES` keeps binary → engine type. Its default-endpoint column goes away.
A candidate whose argv carries no port contributes PIDs (and therefore GPU
attribution and log tails) but no endpoint of its own.

**2. Engines announce the port they are reached on, in their argv.**
Detection accepts a small set of port flag names — `--port`, `--front-port`,
`--api-port`, `-p` — and treats the value as "where this process answers HTTP".
For Strata that means the front passes its port down to the native engine
(`serve/server.py:664` builds the engine's argv); Strata's engine needs a matching
flag, because its parser exits 2 on an unknown argument
(`src/program/generate.cpp:2100`). That is a cross-repo change and is tracked as a
dependency of this one, not as part of it.

**3. When one PID tree carries several ports, the one that answers wins.**
Probe each port found in the tree, keep the endpoint whose `/health` answers, and
merge every PID in the tree onto it. The merge already happens; what is new is that
the choice is explicit and logged at `info` with the port that won and the ones it
beat.

**4. Removal is reversible and visible.**
An engine dropped by the 30 s grace period must be re-added by the next detection
pass that sees it, and both the removal and the re-add carry a reason in the log.
A restart of an engine is routine; the dashboard must survive it without an
operator noticing.

**5. An engine with no resolvable endpoint is shown, not hidden.**
The UI lists it with its type, its PIDs and its GPU badge, and states why there are
no engine numbers ("no port found in its command line") instead of the engine
silently not being there. Silent absence is the failure this ADR exists to remove.

## Rejected alternatives

- **Ask the OS which port the process listens on.** Correct in principle, impossible
  in practice: it needs `/proc/<pid>/fd`, which is `0500`. It would mean telling
  operators to run the dashboard as root.
- **Read the engine's environment.** `/proc/<pid>/environ` is `0400`. Same problem.
- **Discovery files** (an engine writes `/run/strata/<name>.json`). World-readable
  and cross-user, but it is new code on both sides, a path to agree on per engine,
  and stale files to clean up after a crash. argv already reaches every scanner.
- **Move the defaults into config** (`SPARK_DASHBOARD_ENGINE_URL`, or a per-engine
  port list). This is the same hardcoded port in a different file: it goes stale the
  moment the operator changes `--port`, and it is one more thing to keep in sync per
  host. It stays as the documented escape hatch, not as the mechanism.
- **Scan a port range.** Guessing, slower and noisier than the guess it replaces.

## Consequences

- **An engine that announces nothing is still found** — by type, with PIDs and GPU
  attribution — and its panel says the endpoint is unknown. That is a visible gap
  where today there is a wrong guess or an empty list.
- **The default-endpoint column must go, in two steps.** Keep it as a fallback for
  one release with a `debug` line naming it as the source, then delete it. Shipping
  the deletion first would drop llama-server and vLLM, which do not announce a port
  yet, from every dashboard on the upgrade.
- **Detection gets a second probe per candidate** where a tree carries more than one
  port. Bounded by the size of the tree, and only for multi-process engines.
- **A cross-repo dependency.** Point 2 needs the engine to cooperate. Strata is the
  one this report is about; llama.cpp and vLLM already carry `--port` in argv, so
  they need nothing.
- **The removal path becomes observable.** Today `engine_map.retain` is silent, which
  is why the reported case took a manual investigation to see at all.

## Tests (per project rules — tests ship with the change)

- `parse_endpoint_from_args`: each accepted flag name; a process with none of them;
  a `--port` whose value is not a number.
- One PID tree, two ports, only the second answering `/health` → one engine, the
  right endpoint, both PIDs attached.
- One PID tree, no port anywhere → an engine of the right type with no endpoint, and
  the reason the UI shows.
- Re-detection: an engine removed by the grace period is back after the next pass,
  with the model re-resolved once (the `invalidate_model_cache` path).
- `frontend`: if the snapshot gains an "endpoint unknown" reason, update
  `frontend/src/types/metrics.ts`, `frontend/src/lib/format.ts`, the panel that
  renders the engine row, and their Vitest specs — the metrics contract in
  `CLAUDE.md` applies.

## Process

Fork branch first (`feat/port-agnostic-engine-discovery` → fork `main`), because the
Strata adapter this builds on exists only on the fork. The upstream PR is a fresh
branch off `origin/main` carrying only the feature commits, per `CLAUDE.md`. If
upstream lands an ADR first, renumber this one — that is what happened to the HEC
ADR (0001 → 0004).
