import { useEffect, useState } from 'react'

/**
 * The running binary's version, in the masthead between the title and the
 * page tabs. Fetched once — a version does not change while the page is up —
 * and renders nothing when the read fails, so a header that cannot reach the
 * server shows no version it cannot stand behind.
 *
 * Sized and coloured to sit beside the masthead title: the title's own size,
 * and the yellow the line charts use for their series.
 */
export function VersionBadge() {
  const [version, setVersion] = useState<string | null>(null)

  useEffect(() => {
    let live = true
    fetch('/api/version')
      .then((response) =>
        response.ok ? response.text() : Promise.reject(new Error(`HTTP ${response.status}`)),
      )
      .then((text) => {
        if (live && text) setVersion(text)
      })
      .catch(() => {
        // No version is better than a wrong one.
      })
    return () => {
      live = false
    }
  }, [])

  if (!version) return null
  return (
    <span
      title="Running version"
      className="shrink-0 text-xl"
      style={{ color: '#f59e0b' }}
    >
      {version}
    </span>
  )
}
