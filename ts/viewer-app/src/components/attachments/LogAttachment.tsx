/**
 * A `log` attachment: its lines, filtered by the text typed (case ignored),
 * the count shown beside the whole; its whole size and a link to the whole
 * file, which a long log's two ends name too.
 */
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { formatBytes } from "../../json-size.ts"

export const LogAttachment = ({ text, bytes, href }: {
  readonly text: string
  readonly bytes: number
  readonly href: string
}): JSX.Element => {
  const [filter, setFilter] = useState("")
  const lines = useMemo(() => text.split("\n").filter((line, at, all) => line !== "" || at < all.length - 1), [text])
  const asked = filter.trim().toLowerCase()
  const shown = asked === "" ? lines : lines.filter((line) => line.toLowerCase().includes(asked))
  return (
    <div className="at-log">
      <div className="at-bar">
        <input
          type="search"
          placeholder="filter"
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          aria-label="filter the log's lines"
        />
        <span className="muted">{shown.length}/{lines.length} lines · {formatBytes(bytes)}</span>
        <a href={href} target="_blank" rel="noreferrer">whole ↗</a>
      </div>
      <pre className="log-tail-text">{shown.length === 0 ? <span className="muted">none</span> : shown.join("\n")}</pre>
    </div>
  )
}
