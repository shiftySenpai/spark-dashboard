import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import { VersionBadge } from '@/components/VersionBadge'

beforeEach(() => {
  vi.restoreAllMocks()
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('VersionBadge', () => {
  it('shows the version reported by the server', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('1.2.3', { status: 200 })),
    )
    render(<VersionBadge />)

    const badge = (await screen.findByText('1.2.3')) as HTMLElement
    // Sized like the masthead title, in the line-charts yellow.
    expect(badge.className).toContain('text-xl')
    expect(badge.style.color).toBe('rgb(245, 158, 11)')
  })

  it('renders nothing when the version cannot be read', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('nope', { status: 500 })),
    )
    const { container } = render(<VersionBadge />)

    // Let the (rejected) request settle before asserting the badge never appears.
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalled())
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(container.textContent).toBe('')
  })

  it('renders nothing when the server is unreachable', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => {
      throw new TypeError('network down')
    }))
    const { container } = render(<VersionBadge />)

    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalled())
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(container.textContent).toBe('')
  })
})
