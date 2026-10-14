import { TimeSeriesChart, type ChartSeries } from '@/components/charts/TimeSeriesChart'
import { MetricTile } from '@/components/engines/EnginePanelPrimitives'
import { LatencyModeControl } from '@/components/engines/LatencyModeControl'
import { useLatencyMode } from '@/hooks/useLatencyMode'
import { computeTrend } from '@/lib/engineStats'
import { formatDurationMs, formatTtft, fmtVal } from '@/lib/format'
import { pickLatencyValue, shownLatencyStatistic, type LatencyMode } from '@/lib/latencyMode'
import { supportsCapability } from '@/lib/engineCapabilities'
import type { EngineSeriesName } from '@/lib/metricsHistoryStore'
import type { LatencyPercentiles } from '@/types/metrics'
import { EnginePanelBody } from './EnginePanelBody'
import { engineIdentity } from './engineLabel'
import { EnginePanelNotice } from './PanelNotice'
import { MultiEnginePanelBody } from './MultiEnginePanelBody'
import { useEnginePanel, useEngineRows } from './useEnginePanel'
import type { PanelContentProps } from '../panelRegistry'

/**
 * The latency an engine is serving at: time to first token, end to end, queue
 * wait, and the two per-token measures.
 *
 * Every value and every line follows the same statistic — average, or one of
 * the percentiles. Mixing them within a panel would be the quiet way to
 * misread a tail-latency problem as a healthy average, so the mode is one
 * choice made in the panel's own header.
 *
 * An engine that reports only some of the panel's metric groups gets only
 * those parts: no queue tile without queue time, and — in a percentile mode on
 * an engine reporting means but no percentiles — the means, each labelled as
 * the mean rather than shown under the percentile the header names.
 */
export function EngineLatencyPanel({ panel }: PanelContentProps) {
  const rows = useEngineRows(panel)
  const resolution = useEnginePanel(panel)
  const [mode, setMode] = useLatencyMode()
  if (rows.status === 'rows') {
    // One row per engine, each on its own TTFT. A row follows the panel's
    // statistic unless its engine ships no histograms — llama.cpp rows fall
    // back to the mean, exactly as a pinned panel for it does.
    return (
      <MultiEnginePanelBody
        seriesLabel="TTFT"
        rows={rows.rows}
        pick={(row) => {
          const metric = row.metric
          const rowMode =
            metric && supportsCapability(row.engine.engine_type, 'latencyPercentiles')
              ? mode
              : 'avg'
          const value = metric
            ? pickLatencyValue(rowMode, metric('ttft_ms'), metric('ttft_percentiles'))
            : null
          const series = row.series
          // The tiles the single view renders, at full size, over this row's
          // engine — E2E, queue time, ITL, TPOT and batch size, on the same
          // statistic the row's headline uses.
          const e2e = metric
            ? pickLatencyValue(rowMode, metric('e2e_latency_ms'), metric('e2e_percentiles'))
            : null
          const e2eDisplay = formatDurationMs(e2e)
          const batch = metric ? metric('avg_batch_size') : null
          return {
            value,
            displayValue: metric ? `${fmtVal(value, formatTtft)} ms` : undefined,
            unit: 'ms',
            data: row.series ? row.series(LATENCY_SERIES.ttft[rowMode]) : [],
            tiles: metric ? (
              <div className="grid grid-cols-2 gap-1.5">
                <MetricTile
                  label="TTFT"
                  value={fmtVal(value, formatTtft)}
                  unit="ms"
                  trend={computeTrend(series ? series(LATENCY_SERIES.ttft[rowMode]) : [])}
                  invertTrend
                />
                <MetricTile
                  label="E2E"
                  value={e2eDisplay.value}
                  unit={e2eDisplay.unit}
                  trend={computeTrend(series ? series(LATENCY_SERIES.e2e[rowMode]) : [])}
                  invertTrend
                />
                {supportsCapability(row.engine.engine_type, 'queueTime') && (
                  <MetricTile
                    label="Queue"
                    value={fmtVal(metric('queue_time_ms'), formatTtft)}
                    unit="ms"
                    trend={computeTrend(series ? series('queueTime') : [])}
                    invertTrend
                  />
                )}
                <MetricTile
                  label="ITL"
                  value={fmtVal(
                    pickLatencyValue(
                      rowMode,
                      metric('inter_token_latency_ms'),
                      metric('itl_percentiles'),
                    ),
                    formatTtft,
                  )}
                  unit="ms"
                  trend={computeTrend(series ? series(LATENCY_SERIES.itl[rowMode]) : [])}
                  invertTrend
                />
                <MetricTile
                  label="TPOT"
                  value={fmtVal(
                    pickLatencyValue(rowMode, metric('tpot_ms'), metric('tpot_percentiles')),
                    formatTtft,
                  )}
                  unit="ms"
                  trend={computeTrend(series ? series(LATENCY_SERIES.tpot[rowMode]) : [])}
                  invertTrend
                />
                <MetricTile
                  label="Batch"
                  value={batch !== null ? batch.toFixed(1) : '--'}
                  unit="/step"
                  trend={computeTrend(series ? series('batchSize') : [])}
                />
              </div>
            ) : undefined,
          }
        }}
      />
    )
  }
  if (resolution.status !== 'resolved' && resolution.status !== 'aggregate') {
    return <EnginePanelNotice resolution={resolution} />
  }

  const { metric, series, reports } = resolution
  // Two independent gates. What the engine's adapter declares on its snapshot
  // (ADR-0004) decides whether a figure exists at all — a means-only engine has
  // no percentile to show, and upstream labels the mean it shows instead. What
  // this fork's capability table says decides what the engine type can produce
  // in principle: llama.cpp ships no latency histograms and no queue-time
  // metric, so its panel hides the percentile options and the queue tile rather
  // than offering a statistic that would always read as a dash.
  const engineType = resolution.status === 'resolved' ? resolution.engine.engine_type : null
  const hasPercentiles = supportsCapability(engineType, 'latencyPercentiles')
  const hasQueue = reports('QueueTime') && supportsCapability(engineType, 'queueTime')
  const statistic = shownLatencyStatistic(mode, {
    means: reports('LatencyMeans'),
    percentiles: reports('LatencyPercentiles') && hasPercentiles,
  })
  /** One latency figure in the statistic shown — or null, omitted, when the
   *  engine reports no statistic it could be shown in. */
  const dimension = (
    name: string,
    key: keyof typeof LATENCY_SERIES,
    mean: number | null,
    percentiles: LatencyPercentiles | null,
  ) =>
    statistic && {
      // A mean standing in for the percentile the header names says what it
      // is: under the percentile's heading it would read as a healthy tail.
      label: statistic === mode ? name : `${name} mean`,
      value: pickLatencyValue(statistic, mean, percentiles),
      series: series(LATENCY_SERIES[key][statistic]),
    }
  const ttft = dimension('TTFT', 'ttft', metric('ttft_ms'), metric('ttft_percentiles'))
  const e2e = dimension('E2E', 'e2e', metric('e2e_latency_ms'), metric('e2e_percentiles'))
  const itl = dimension('ITL', 'itl', metric('inter_token_latency_ms'), metric('itl_percentiles'))
  const tpot = dimension('TPOT', 'tpot', metric('tpot_ms'), metric('tpot_percentiles'))
  // Queue wait is a mean in every mode, as it always has been.
  const queueSeries = hasQueue ? series('queueTime') : null
  // Tokens per engine step is a throughput figure rather than a latency, so it
  // follows the throughput group; this panel is only where it is shown.
  const showBatch = reports('Throughput')
  const batchSize = metric('avg_batch_size')

  // TTFT lives on the left axis (typically hundreds of ms); queue, ITL and
  // TPOT share a right axis (often single or double digits) so their variation
  // stays visible against the TTFT scale.
  const lines: Array<ChartSeries | null> = [
    ttft && { data: ttft.series, label: ttft.label, color: '#f59e0b', axis: 'left' },
    queueSeries && { data: queueSeries, label: 'Queue', color: '#8b5cf6', axis: 'right' },
    itl && { data: itl.series, label: itl.label, color: '#14b8a6', axis: 'right' },
    tpot && { data: tpot.series, label: tpot.label, color: '#ec4899', axis: 'right' },
  ]
  const chartSeries = lines.filter((line) => line !== null)

  return (
    <EnginePanelBody
      identity={engineIdentity(resolution)}
      actions={
        <LatencyModeControl
          mode={mode}
          onModeChange={setMode}
          percentilesSupported={hasPercentiles}
        />
      }
      tiles={
        <div className="grid grid-cols-2 gap-1.5">
          {ttft && (
            <MetricTile
              label={ttft.label}
              value={fmtVal(ttft.value, formatTtft)}
              unit="ms"
              trend={computeTrend(ttft.series)}
              invertTrend
            />
          )}
          {e2e && (
            <MetricTile
              label={e2e.label}
              value={formatDurationMs(e2e.value).value}
              unit={formatDurationMs(e2e.value).unit}
              trend={computeTrend(e2e.series)}
              invertTrend
            />
          )}
          {queueSeries && (
            <MetricTile
              label="Queue"
              value={fmtVal(metric('queue_time_ms'), formatTtft)}
              unit="ms"
              trend={computeTrend(queueSeries)}
              invertTrend
            />
          )}
          {itl && (
            <MetricTile
              label={itl.label}
              value={fmtVal(itl.value, formatTtft)}
              unit="ms"
              trend={computeTrend(itl.series)}
              invertTrend
            />
          )}
          {tpot && (
            <MetricTile
              label={tpot.label}
              value={fmtVal(tpot.value, formatTtft)}
              unit="ms"
              trend={computeTrend(tpot.series)}
              invertTrend
            />
          )}
          {showBatch && (
            <MetricTile
              label="Batch"
              value={batchSize !== null ? batchSize.toFixed(1) : '--'}
              unit="/step"
              trend={computeTrend(series('batchSize'))}
            />
          )}
        </div>
      }
      chart={
        chartSeries.length > 0 ? (
          <TimeSeriesChart hideTooltipLabel series={chartSeries} unit="ms" />
        ) : undefined
      }
    />
  )
}

/**
 * Which series carries each latency dimension under each statistic. The
 * percentile series are ingested alongside the averages, so switching the mode
 * re-reads history rather than starting a new one.
 */
const LATENCY_SERIES = {
  ttft: { avg: 'ttft', p50: 'ttftP50', p95: 'ttftP95', p99: 'ttftP99' },
  itl: { avg: 'interTokenLatency', p50: 'itlP50', p95: 'itlP95', p99: 'itlP99' },
  e2e: { avg: 'e2eLatency', p50: 'e2eP50', p95: 'e2eP95', p99: 'e2eP99' },
  tpot: { avg: 'tpot', p50: 'tpotP50', p95: 'tpotP95', p99: 'tpotP99' },
} as const satisfies Record<string, Record<LatencyMode, EngineSeriesName>>
