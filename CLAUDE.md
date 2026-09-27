# spark-dashboard — Claude project rules

Project-specific. Global rules in `~/.claude/rules/` still apply.

## Branches & commits

- Remotes: `origin` = **upstream** (niklasfrick/spark-dashboard), `fork` = shiftySenpai/spark-dashboard. Day-to-day work lands on the fork's `main` directly: `<type>/<slug>` branch (`feat/...`, `fix/...`, `docs/...`) → rebase onto `main` → `git merge --ff-only` → push to `fork`.
- Finished features are contributed **upstream** as PRs. An upstream PR is a **fresh branch cut from latest `origin/main`** carrying only the feature's commits (cherry-picked, fork-only chores/docs skipped) — never the fork's `main`, whose history contains duplicated HEC commits, upstream-sync merges, and fork tooling. In flight (2026-09): **HEC export = upstream PR #125** (rebuilt onto current upstream main; ADR renumbered 0001 → 0004 because upstream owns 0001–0003; old PR #97 closed as superseded), then one **"engines and dashboard" PR** (llama.cpp + multi-GPU + engine status/rows) stacked on it — coordinate with upstream PR #122 (GPU PCIe panel; overlaps `src/metrics/gpu.rs`, `frontend/src/types/metrics.ts`, the panel registry; #122's branch is merged into our `main` pre-merge for verification — rebase the engines PR onto upstream main once #122 lands).
- Fork-only files never go upstream: `CLAUDE.md` edits, `docs/repo-graph.md`, `.gitignore` additions, spec files (`LlamaCppEngine-spec.md`, `SplunkHEC-spec.md`), `Splunk-App/`, `graphify-out/`.
- Every commit must be a valid Conventional Commit (release-please reads commits in **both** repos — never hand-edit version fields, `.release-please-manifest.json`, or `CHANGELOG.md`).
- Run the pre-commit checks below before pushing. GitHub Actions is not enabled on this fork; upstream runs its own CI on PRs.

## Commits drive releases

`release-please` reads commits on `main` to bump versions and publish to crates.io. Format: `<type>(<scope>)<!>: <description>`.

| Type                                                       | Bump (pre-1.0)                  |
| ---------------------------------------------------------- | ------------------------------- |
| `feat:`                                                    | minor                           |
| `fix:`                                                     | patch                           |
| `feat!:` / `BREAKING CHANGE:`                              | minor (becomes major after 1.0) |
| `chore`, `docs`, `refactor`, `test`, `ci`, `perf`, `style` | none                            |

"Bump" is version impact only — `chore`/`deps` still appear in the changelog under "Dependencies & Chores" (see `changelog-sections` in `release-please-config.json`); only `docs`/`style`/`refactor`/`test`/`build`/`ci` stay hidden.

Tags: `vX.Y.Z`. After merge, release-please opens a rolling release PR; merging it tags + triggers `publish.yml` (`cargo publish`).

**Never hand-edit the release-please-owned bits**: the `version` fields of `Cargo.toml` and `frontend/package.json`, `.release-please-manifest.json`, and `CHANGELOG.md`. Dependency changes to those same files are fine when driven through the proper tooling (`cargo update`/`cargo add`, `npm install`/`npm update` — which also rewrite `Cargo.lock`/`frontend/package-lock.json`); just leave the `version` fields untouched.

## Pre-commit checks (run before pushing)

Rust changes (`src/`, `Cargo.*`):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Frontend changes (`frontend/`):

```bash
cd frontend && npm run lint && npm run build && npm test -- --run

# only when a *.browser.test.tsx changed (one-time: npx playwright install chromium)
cd frontend && npm run test:browser
```

`npm run lint` is enforced by the `frontend` CI job and the baseline is clean — a new error fails the build, so don't let one land.

Vitest runs two projects, so `npm test` alone does not cover both. `npm test` is `unit` (jsdom); specs named `*.browser.test.tsx` belong to `browser`, run in headless chromium, and are enforced by the `frontend-browser` CI job. Put a spec there only when it depends on real layout or CSS — jsdom measures every box as 0×0 — and keep that project small, since it costs a browser download.

If both stacks changed, run both blocks. If embedded assets changed, build the frontend first (`rust-embed` needs `frontend/dist/`).

Docker changes (`deploy/docker/Dockerfile`, `deploy/docker/docker-compose*.yml`):

```bash
./dev/docker-dev.sh --build-local   # buildx multi-stage build smoke test (no GPU)
```

## Dependencies — pick the latest stable

When a dependency is **introduced or selected for the first time** — a crate, npm
package, Docker base image, GitHub Action, toolchain version, anything pinned —
check its newest/latest **stable** release first and pin to that, rather than
copying an older version from memory or an existing line. Verify against the
source of truth (crates.io / npm / the registry's tags / upstream releases), not
training-data recall.

Pick the latest stable available for that distribution channel — and actually
look it up. (Lesson learned the hard way: Google distroless's newest Debian
variant is `-debian13`/trixie, which is also its default — not `-debian12`, which
recall wrongly insisted was the newest. The registry/README is the source of
truth.) State the version you picked and why in the PR/commit.

## Metrics contract (Rust ↔ frontend)

When you change `MemoryMetrics`/`GpuMetrics`/`CpuMetrics` shape, serde names, display logic, or fields — update all of these in the same PR:

1. Rust unit tests in `src/metrics/`
2. TS types in `frontend/src/types/metrics.ts`
3. Formatters in `frontend/src/lib/format.ts`
4. Vitest specs in `frontend/src/__tests__/`
5. Components in `frontend/src/components/`

If one is genuinely N/A, say so in the commit.

## Dashboard schema versioning

`DASHBOARD_SCHEMA_VERSION` (`frontend/src/lib/dashboard/schema.ts`) is bumped for **every** document-shape change — additive ones included (policy adopted with upstream v0.14.0). A bump without a migration protects the new field from an older build's lossy save. In the same PR: the version bump, a migration step in `migrations.ts` (identity for additive changes), and a version test in `schema.test.ts`.

## Tests ship with the change

No behavior change merges without test coverage in the same PR. Rust branches → `#[cfg(test)]`. Frontend components/formatters → Vitest. New API field → both sides.

## Agent skills

### Issue tracker

Issues live in GitHub Issues (`gh` CLI); external PRs are also a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`); existing `wontfix` label reused. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
