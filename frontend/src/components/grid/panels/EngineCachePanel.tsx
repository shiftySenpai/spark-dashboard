import { TimeSeriesChart, type ChartSeries } from '@/components/charts/TimeSeriesChart'
import { AnimatedCounter } from '@/components/engines/AnimatedCounter'
import { KvBar, MetricTile, TrendArrow } from '@/components/engines/EnginePanelPrimitives'
import { computeTrend } from '@/lib/engineStats'
import { supportsCapability } from '@/lib/engineCapabilities'
import { formatCompactTokens } from '@/lib/format'
import { EnginePanelBody } from './EnginePanelBody'
import { engineIdentity } from './engineLabel'
import { EnginePanelNotice } from './PanelNotice'
import { MultiEnginePanelBody } from './MultiEnginePanelBody'
import { useEnginePanel, useEngineRows } from './useEnginePanel'
import type { PanelContentProps } from '../panelRegistry'

/**
 * How much of the engine's KV cache is in use, and how much of the prompt
 * traffic the prefix cache is answering.
 *
 * The two belong together: a KV cache filling up is what limits how many
 * requests an engine can hold, and prefix hits are what keep it from filling.
 * They are separate metric groups all the same, so an engine reporting one of
 * them gets that half of the panel — never a dash standing in for the other.
 */
export function EngineCachePanel({ panel }: PanelContentProps) {
  const rows = useEngineRows(panel)
  const resolution = useEnginePanel(panel)
  if (rows.status === 'rows') {
    // One row per engine. KV-cache usage is a vLLM gauge; a llama.cpp row
    // shows its prefix hit rate instead, so the rows are labelled which they
    // are — the same reason the single view hides the KV block for it.
    return (
      <MultiEnginePanelBody
        seriesLabel="Cache"
        rows={rows.rows}
        pick={(row) => {
          const metric = row.metric
          const kv = metric ? supportsCapability(row.engine.engine_type, 'kvCache') : false
          const value = !metric ? null : kv ? metric('kv_cache_percent') : metric('prefix_cache_hit_rate')
          return {
            value,
            displayValue: metric && value !== null ? `${Math.round(value)}%` : undefined,
            valueLabel: metric ? (kv ? 'KV' : 'Prefix') : undefined,
            unit: '%',
            data: row.series ? row.series(kv ? 'kvCache' : 'prefixCacheHit') : [],
            yDomain: [0, 100],
          }
        }}
      />
    )
  }
  if (resolution.status !== 'resolved' && resolution.status !== 'aggregate') {
    return <EnginePanelNotice resolution={resolution} />
  }

  const { metric, series, reports } = resolution
  // Two gates on each half, both of which must hold to draw it: what the
  // engine's adapter declares on its snapshot (ADR-0004), and what this fork
  // knows the engine type can produce at all — llama.cpp has no KV-usage
  // gauge, so its block is hidden rather than rendered as a "--%" that reads
  // as a fault.
  const engineType = resolution.status === 'resolved' ? resolution.engine.engine_type : null
  const kvCache = reports('KvCache') && supportsCapability(engineType, 'kvCache')
  const prefixCache = reports('PrefixCache')
  const kvPercent = metric('kv_cache_percent')
  const prefixHit = metric('prefix_cache_hit_rate')
  const kvSeries = series('kvCache')

  const lines: Array<ChartSeries | null> = [
    kvCache ? { data: kvSeries, label: 'KV Cache', color: '#76B900' } : null,
    prefixCache ? { data: series('prefixCacheHit'), label: 'Prefix Hit', color: '#3b82f6' } : null,
  ]

  return (
    <EnginePanelBody
      identity={engineIdentity(resolution)}
      tiles={
        <div className="flex flex-col gap-2">
          <div className="grid grid-cols-2 gap-1.5">
            {kvCache && (
              <div className="flex flex-col gap-0.5 min-w-0">
                <span className="text-[10px] font-medium text-zinc-400 uppercase tracking-wider truncate">
                  KV Cache
                </span>
                <div className="flex items-baseline">
                  <span className="text-lg xl:text-xl 2xl:text-2xl min-[1920px]:text-3xl font-bold text-zinc-100 font-mono tabular-nums leading-none">
                    {kvPercent !== null ? Math.round(kvPercent) : '--'}
                  </span>
                  <span className="text-xs text-zinc-500 ml-1">%</span>
                  {/* A filling cache is bad news, so the arrow's colours invert. */}
                  <TrendArrow trend={computeTrend(kvSeries)} invertColor />
                </div>
                {kvPercent !== null && <KvBar percent={kvPercent} />}
              </div>
            )}
            {prefixCache && (
              <MetricTile
                label="Prefix Hit"
                value={prefixHit !== null ? `${Math.round(prefixHit)}` : '--'}
                unit="%"
              />
            )}
          </div>
          {prefixCache && (
            <div className="flex flex-col gap-0.5 min-w-0">
              <span className="text-[10px] 2xl:text-xs font-medium text-zinc-400 uppercase tracking-wider truncate">
                Prefix Queries
              </span>
              <AnimatedCounter
                value={metric('prefix_cache_queries_total')}
                format={formatCompactTokens}
                className="text-lg xl:text-xl 2xl:text-2xl min-[1920px]:text-3xl font-bold text-zinc-100 font-mono tabular-nums leading-none"
              />
            </div>
          )}
        </div>
      }
      chart={
        <TimeSeriesChart
          hideTooltipLabel
          series={lines.filter((line) => line !== null)}
          yDomain={[0, 100]}
          unit="%"
        />
      }
    />
  )
}
