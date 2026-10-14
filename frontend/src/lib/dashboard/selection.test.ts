import { describe, expect, it } from 'vitest'
import {
  NO_SELECTION,
  pageSelection,
  withSelectedEngine,
  withSelectedGpu,
} from './selection'
import { ALL_MODELS } from './pageSource'
import type { EngineSnapshot, GpuMetrics, MetricsSnapshot } from '@/types/metrics'

function gpu(index: number | null): GpuMetrics {
  return {
    index,
    name: 'NVIDIA GB10',
    utilization_percent: 10,
    memory_total_bytes: null,
    memory_used_bytes: null,
    temperature_celsius: 40,
    power_watts: 50,
    power_limit_watts: null,
    clock_graphics_mhz: null,
    clock_sm_mhz: null,
    clock_memory_mhz: null,
    fan_speed_percent: null,
    pcie_rx_bytes_per_sec: null,
    pcie_tx_bytes_per_sec: null,
  }
}

function engine(endpoint: string, status: EngineSnapshot['status'] = { type: 'Running' }): EngineSnapshot {
  return {
    engine_type: 'Vllm',
    endpoint,
    status,
    model: null,
    metrics: null,
    recent_requests: [],
    deployment_mode: 'Native',
  }
}

function snapshot(gpus: GpuMetrics[], engines: EngineSnapshot[]): Pick<
  MetricsSnapshot,
  'gpu' | 'gpus' | 'engines'
> {
  return { gpu: gpus[0], gpus, engines }
}

describe('pageSelection', () => {
  it('follows every GPU and the running engine when nothing is chosen or configured', () => {
    // This is what makes the shipped preset work unmodified on any machine. On a
    // multi-GPU host the default target is "all" GPUs — the divided view.
    const host = snapshot([gpu(0), gpu(1)], [engine('http://localhost:8000')])

    expect(pageSelection(host, {})).toEqual({
      gpuTarget: { kind: 'all' },
      engineTarget: { kind: 'engine', endpoint: 'http://localhost:8000' },
    })
  })

  it('defaults a one-GPU host to that GPU', () => {
    const host = snapshot([gpu(0)], [engine('http://localhost:8000')])

    expect(pageSelection(host, {}).gpuTarget).toEqual({ kind: 'gpu', index: 0 })
  })

  it('follows every engine on a multi-engine host when nothing is chosen or configured', () => {
    // The host's default is the one-row-per-engine view — the engine
    // counterpart of the multi-GPU default — not one engine standing in for
    // the rest. A stopped engine still gets its row.
    const host = snapshot(
      [gpu(0)],
      [engine('http://localhost:8000', { type: 'Stopped' }), engine('http://localhost:8001')],
    )

    expect(pageSelection(host, {}).engineTarget).toEqual({ kind: 'all' })
  })

  it('still follows every engine when none of them is running', () => {
    // The target stays "all"; a panel on such a page degrades to the
    // "no engine running" state, the same as a page configured for all models.
    const host = snapshot(
      [gpu(0)],
      [
        engine('http://localhost:8000', { type: 'Loading' }),
        engine('http://localhost:8001', { type: 'Stopped' }),
      ],
    )

    expect(pageSelection(host, {}).engineTarget).toEqual({ kind: 'all' })
  })

  it('normalizes the index of a GPU reported without one', () => {
    expect(pageSelection(snapshot([gpu(null)], []), {}).gpuTarget).toEqual({
      kind: 'gpu',
      index: 0,
    })
  })

  it('selects nothing for an engine the host is not running', () => {
    // Engine panels degrade to an empty state here; the hardware panels beside
    // them keep working, which is the point.
    expect(pageSelection(snapshot([gpu(0)], []), {}).engineTarget).toBeNull()
  })

  it('resolves a page configured for one engine to that engine', () => {
    const host = snapshot(
      [gpu(0)],
      [engine('http://localhost:8000'), engine('http://localhost:8001')],
    )

    expect(
      pageSelection(host, {}, { kind: 'engine', endpoint: 'http://localhost:8001' }).engineTarget,
    ).toEqual({ kind: 'engine', endpoint: 'http://localhost:8001' })
  })

  it('resolves a page configured for all models to the aggregate', () => {
    const host = snapshot([gpu(0)], [engine('http://localhost:8000')])

    expect(pageSelection(host, {}, ALL_MODELS).engineTarget).toEqual({ kind: 'all' })
  })

  it('keeps a configured engine whose target has gone away', () => {
    // Panels report it as missing. Nudging the page back to whatever is
    // running would hide from the operator that the engine their page names
    // has stopped — and would show its numbers under the other engine's name.
    const host = snapshot([gpu(0)], [engine('http://localhost:8000')])

    expect(
      pageSelection(host, {}, { kind: 'engine', endpoint: 'http://localhost:9999' }).engineTarget,
    ).toEqual({ kind: 'engine', endpoint: 'http://localhost:9999' })
  })

  it('honours what the operator chose over the host defaults', () => {
    const host = snapshot(
      [gpu(0), gpu(1)],
      [engine('http://localhost:8000'), engine('http://localhost:8001')],
    )

    expect(
      pageSelection(host, {
        gpuTarget: { kind: 'gpu', index: 1 },
        engineTarget: { kind: 'engine', endpoint: 'http://localhost:8001' },
      }),
    ).toEqual({
      gpuTarget: { kind: 'gpu', index: 1 },
      engineTarget: { kind: 'engine', endpoint: 'http://localhost:8001' },
    })
  })

  it('honours a session choice over the configured source', () => {
    // The source is the page's default; a choice made while looking at the
    // page is the operator asking to see something else right now.
    const host = snapshot(
      [gpu(0)],
      [engine('http://localhost:8000'), engine('http://localhost:8001')],
    )

    expect(
      pageSelection(
        host,
        { engineTarget: { kind: 'engine', endpoint: 'http://localhost:8000' } },
        ALL_MODELS,
      ).engineTarget,
    ).toEqual({ kind: 'engine', endpoint: 'http://localhost:8000' })
  })

  it('honours a session choice of all models over a source naming one engine', () => {
    // The other direction, and the one engine rotation starts every cycle
    // with: the session can ask for the combined view on a one-engine page.
    const host = snapshot(
      [gpu(0)],
      [engine('http://localhost:8000'), engine('http://localhost:8001')],
    )

    expect(
      pageSelection(
        host,
        { engineTarget: { kind: 'all' } },
        { kind: 'engine', endpoint: 'http://localhost:8001' },
      ).engineTarget,
    ).toEqual({ kind: 'all' })
  })

  it('honours a session choice of all models on a host left to its defaults', () => {
    const host = snapshot([gpu(0)], [engine('http://localhost:8000')])

    expect(pageSelection(host, { engineTarget: { kind: 'all' } }).engineTarget).toEqual({
      kind: 'all',
    })
  })

  it('keeps a choice whose target has gone away', () => {
    const host = snapshot([gpu(0)], [engine('http://localhost:8000')])

    expect(
      pageSelection(host, {
        gpuTarget: { kind: 'gpu', index: 3 },
        engineTarget: { kind: 'engine', endpoint: 'http://localhost:9999' },
      }),
    ).toEqual({
      gpuTarget: { kind: 'gpu', index: 3 },
      engineTarget: { kind: 'engine', endpoint: 'http://localhost:9999' },
    })
  })

  it('has nothing to select before the first snapshot', () => {
    expect(pageSelection(null, {})).toEqual(NO_SELECTION)
  })

  it('keeps a choice made before the first snapshot arrives', () => {
    expect(pageSelection(null, { gpuTarget: { kind: 'gpu', index: 1 } })).toEqual({
      gpuTarget: { kind: 'gpu', index: 1 },
      engineTarget: null,
    })
  })

  it('resolves a configured source before the first snapshot arrives', () => {
    // A page configured for all models must not flash the host default while
    // the socket connects.
    expect(pageSelection(null, {}, ALL_MODELS).engineTarget).toEqual({ kind: 'all' })
  })
})

describe('choosing what a page points at', () => {
  it('records a chosen GPU and engine', () => {
    expect(withSelectedGpu({}, { kind: 'gpu', index: 2 })).toEqual({
      gpuTarget: { kind: 'gpu', index: 2 },
    })
    expect(
      withSelectedEngine(
        { gpuTarget: { kind: 'gpu', index: 2 } },
        { kind: 'engine', endpoint: 'http://localhost:8000' },
      ),
    ).toEqual({
      gpuTarget: { kind: 'gpu', index: 2 },
      engineTarget: { kind: 'engine', endpoint: 'http://localhost:8000' },
    })
  })

  it('records all models as a choice of its own', () => {
    const onGpu2 = { gpuTarget: { kind: 'gpu', index: 2 } as const }
    expect(withSelectedEngine(onGpu2, { kind: 'all' })).toEqual({
      ...onGpu2,
      engineTarget: { kind: 'all' },
    })
    expect(
      withSelectedEngine({ engineTarget: { kind: 'engine', endpoint: 'http://x:1' } }, {
        kind: 'all',
      }),
    ).toEqual({ engineTarget: { kind: 'all' } })
  })

  it('goes back to following the host when the choice is cleared', () => {
    // Absent means "never chose", which is what a cleared selection has to
    // become — storing a null would freeze the page on a host that changes.
    const target = { kind: 'engine', endpoint: 'http://x:1' } as const
    const chosen = { gpuTarget: { kind: 'gpu', index: 2 } as const, engineTarget: target }
    expect(withSelectedGpu(chosen, null)).toEqual({
      engineTarget: target,
    })
    expect(withSelectedEngine(chosen, null)).toEqual({
      gpuTarget: { kind: 'gpu', index: 2 },
    })
    // Clearing all models is clearing a choice like any other.
    expect(withSelectedEngine({ engineTarget: { kind: 'all' } }, null)).toEqual({})
  })

  it('leaves the selection it was given untouched', () => {
    const chosen = { gpuTarget: { kind: 'gpu', index: 0 } } as const
    withSelectedGpu(chosen, { kind: 'gpu', index: 1 })
    expect(chosen).toEqual({ gpuTarget: { kind: 'gpu', index: 0 } })
  })
})
