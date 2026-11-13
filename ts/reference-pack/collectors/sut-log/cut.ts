/**
 * The `sut-log` collector's cut: a log's lines as each cell owns them. A line
 * opening with an RFC 3339 instant (the B2BUA runner's `tracing` format)
 * starts an entry; a line opening otherwise continues the entry before it. A
 * cell owns each entry stamped within its window, and each naming one of its
 * Call-IDs wherever it is stamped.
 */

/** A cell as the cut reads it: its window in wall-clock milliseconds and its calls' Call-IDs. */
export interface Window {
  readonly startMs: number
  readonly endMs: number
  readonly callIds: ReadonlyArray<string>
}

/** One entry of the log: the instant it is stamped with, where it opens with one, and its text. */
export interface Entry {
  readonly atMs: number | undefined
  readonly text: string
}

const STAMP = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2}))\s/

/** The entries of `text`, in order. */
export const entriesOf = (text: string): ReadonlyArray<Entry> => {
  const entries: Array<{ atMs: number | undefined; text: string }> = []
  for (const line of text.split("\n")) {
    if (line === "") continue
    const stamp = STAMP.exec(line)
    const previous = entries.at(-1)
    if (stamp === null && previous !== undefined) {
      previous.text = `${previous.text}\n${line}`
      continue
    }
    entries.push({ atMs: stamp === null ? undefined : Date.parse(stamp[1]!), text: line })
  }
  return entries
}

/** The entries of `entries` that `window` owns, as one text. */
export const cut = (entries: ReadonlyArray<Entry>, window: Window): string =>
  entries
    .filter((entry) =>
      (entry.atMs !== undefined && entry.atMs >= window.startMs && entry.atMs <= window.endMs) ||
      window.callIds.some((id) => entry.text.includes(id))
    )
    .map((entry) => `${entry.text}\n`)
    .join("")
