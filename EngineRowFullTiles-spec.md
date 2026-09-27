# Engine Row Full Tiles — Feature Spec

Working design doc for replacing the compact per-row stat line in the
all-models (one-row-per-engine) view with the single view's full-size
tiles. Follows `LlamaCppEngine-spec.md` / `SplunkHEC-spec.md` convention
(untracked working doc).

Status: **not started.** Baseline state: `main` at `5059480`
(feat "keep every panel's figures on its engine rows").

## 1. Goal

On a page showing every engine, each engine's row must carry the **same
figures the single view shows, at full tile size** — not the current
compact 10px stat line. A row should read like a mini version of the
single-engine card: identity header, full tiles, chart.

Example — Decode Throughput row (today → target):

```
Today (compact line):                          Target (full tiles):
Qwen3-8B · vLLM · :8000        140.0 tok/s     Qwen3-8B · vLLM · :8000   140.0 tok/s
Avg 100.0 tok/s · Per-Req 40.0 tok/s · …        ┌──────┬────────┐
[chart: 3 lines]                                 │ Live │Generated│
                                                 └──────┴────────┘
                                                 Avg      Per-Req Avg
                                                 100.0    40.0
                                                 [chart: 3 lines]
```

## 2. Current state (verified, `main` @ 5059480)

- `frontend/src/components/grid/panels/MultiEnginePanelBody.tsx` — the
  shared row body. Each row renders:
  1. header: provider mark, engine chip, model name, endpoint,
     `displayValue` (big number, right);
  2. optional **compact stat line**: `value.stats`
     (`{label, value}[]`) as one `text-[10px] tabular-nums text-zinc-500`
     line (this is what this spec replaces);
  3. optional multi-series chart: primary `value.data` (labelled by
     `seriesLabel`, color `#76B900`) plus `value.series` extras.
- Row data comes from `useEngineRows(panel)` → `EngineRowTarget`
  (`useEnginePanel.ts`): `engine`, `key`, `metric` (the engine's
  `EngineMetricReader`), `series` (per-engine history lookup), `note`.
  **The data layer is complete — this change is purely presentational.**
- Single-view tiles use the shared primitives in
  `frontend/src/components/engines/EnginePanelPrimitives.tsx`:
  `LiveWithTotal`, `MetricTile`, `KvBar`, `GoodputTile`,
  `SpecDecodeSection` (all exported from that file).
- Compact-stat wiring today (to be replaced by tiles):
  - `EngineThroughputPanel.tsx` row pick: stats Avg / Per-Req Avg /
    `{totalLabel}`; series Avg `#3b82f6` + Per-req `#a855f7`.
  - `EngineLatencyPanel.tsx` row pick: stats E2E / Queue / ITL / TPOT /
    Batch.
  - `EngineRequestsPanel.tsx` row pick: stats Queued / Total /
    Swapped+ / Preempt+.
  - `EngineSloGoodputPanel.tsx` row pick: stats TTFT / ITL / E2E %;
    `data: []` (the SLO panel has no chart in either view).
  - `EngineCachePanel.tsx` / `EngineSpecDecodePanel.tsx` row picks pass
    no stats (rows already show the panel's whole figure) — unchanged.

## 3. Target behavior

Per row (one per engine), top to bottom:

1. **Header — unchanged**: identity marks + `displayValue` kept. The
   headline stays in the header as the row's at-a-glance figure; the
   tiles below are the detail. (Do not move the headline into a tile.)
2. **Tile block — new, replaces the stat line**: the exact tile JSX the
   panel's single view renders, built from the shared primitives and
   parameterised on the row's `metric`/`series` (same values, same
   labels, same units, same conditionals as the single view).
3. **Chart — kept**: the multi-series chart stays as-is (see §2). The
   SLO panel keeps `data: []` (renders nothing).
4. **Non-serving rows — unchanged**: a row with `note` (starting /
   metrics disabled / offline) shows its note instead of tiles + chart,
   exactly as today.

### 3.1 Tile inventory per panel (mirror the single view)

| Panel | Row tiles (single view's `tiles` block) |
|---|---|
| Decode / Prefill Throughput | `LiveWithTotal` (live + `Generated`/`Processed` total) + `MetricTile` Avg + `MetricTile` Per-Req Avg |
| Latency | `grid grid-cols-2` of `MetricTile`: TTFT, E2E, Queue (only when `supportsCapability(type, 'queueTime')`), ITL, TPOT, Batch — same percentile-mode selection as the row's headline (`rowMode`) |
| Requests | `grid grid-cols-2` of `MetricTile`: Active, Queued, Total, + Swapped / Preempt only when `> 0` (`warn`) |
| SLO Goodput | `GoodputTile` grid: Combined (col-span-2, emphasize) + TTFT/ITL/TPOT/E2E per-SLO tiles, scored against the default thresholds (the row already computes `ttft/itl/e2e` against `DEFAULT_SLO`; add `tpot` the same way) |
| Cache, Spec Decode | unchanged (no stats today; no change) |

## 4. Implementation notes

- **Mechanism**: add `tiles?: ReactNode` to `MultiEngineRowValue`
  (`MultiEnginePanelBody.tsx`) and render it between header and chart.
  Delete the `stats` field and its compact-line rendering — it is
  superseded wholesale, do not keep both.
- Each panel's row pick builds its tile block from the primitives
  (copy the single view's `tiles={...}` JSX, swapping `metric`/`series`
  for the row's). Keep both views honest the way the throughput pair
  already is: the row tiles must not drift from the single view's.
- **Row sizing**: rows are currently `flex-1` inside the panel
  (`overflow-hidden`). With a tile block that no longer splits cleanly —
  drop `flex-1`, let rows size to content, and give the chart a fixed
  height (suggest `h-20` or `h-24`) so rows align. Note text in place of
  a chart is fine at its natural height.
- **Panel geometry**: the shipped preset gives engine panels
  `h: 3` (`frontend/src/lib/dashboard/preset.ts`). Two rows of
  header + tiles + `h-20` chart will not fit `h: 3` on a 1080p-height
  row. If they don't fit (check at ~1080p viewport), raise the preset's
  engine-panel geometry (and the `storedDocument` defaults in the test
  harnesses) accordingly. Stored documents keep their own geometry —
  no schema change, no migration, `DASHBOARD_SCHEMA_VERSION` untouched.
  (Only document-shape changes bump the schema — this is not one.)
- Non-goals: no data-layer or backend changes; single-engine hosts,
  pinned panels, pinned pages, and the explicit all-models page source
  keep their exact behavior apart from the tile upgrade itself; the
  "All Engine" status panel is untouched.

## 5. Tests (ship with the change)

`frontend/src/__tests__/App.enginePanels.test.tsx` — the row specs
(describe `engine panels on a page configured for all models` +
`engine panels on an unconfigured page of a multi-engine host`):

- Compact-stat assertions become tile assertions. `MetricTile` renders
  value and unit in **separate spans** — so e.g.
  `getAllByText('100.0 tok/s')` becomes `getAllByText('100.0')`
  (×2 engines) with the unit rendered beside it; `500K tok` splits into
  `500K` + `tok`; the `Avg` / `Per-Req Avg` labels now live in the
  tiles' label spans.
- Chart assertions via the mocked `chart-series-<label>` testids
  (e.g. `chart-series-Decode throughput`, `chart-series-Avg`,
  `chart-series-Per-req`) stay valid.
- Add one spec asserting the per-panel tile inventory (§3.1) for at
  least the throughput row: both engines' rows contain the
  `Generated`/`Processed` total tile and Avg / Per-Req tiles.
- No browser-project test needed: the change is content/height, not
  measured layout. Keep `*.browser.test.tsx` small per repo rules.

## 6. Gates & landing (repo convention — `CLAUDE.md`)

```bash
cd frontend && npm run lint && npm run build && npm test -- --run
```

No `*.browser.test.tsx` changes → no `test:browser`. Frontend-only, so
no Rust gates. Work on a `feat/<slug>` branch, rebase onto `main`,
`git merge --ff-only`, push `main` to the `fork` remote
(`shiftySenpai/spark-dashboard`; `origin` is upstream and read-only).
Conventional Commit (`feat(dashboard): …`). Run `graphify update .`
after.
