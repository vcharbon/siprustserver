/**
 * The live log (§6 J4): the file tailed by byte offset every 1.5 s — the
 * page asks from where the last answer ended, never for the whole file (§8
 * E4) — scrolled to its end as it grows, with a pause that holds both the
 * polling and the scroll. Ends by itself once the job has ended and the end
 * was read. The folding of answers is `log-tail.ts`.
 */
import type { JSX } from "react"
import { useEffect, useRef, useState } from "react"
import { api } from "../api.ts"
import { EMPTY, fold, nextDelay, type Tail } from "../log-tail.ts"

const CADENCE_MS = 1_500

export const LogTail = ({ id, path }: { readonly id: number; readonly path: string | null }): JSX.Element => {
  const [tail, setTail] = useState<Tail>(EMPTY)
  const [paused, setPaused] = useState(false)
  const [error, setError] = useState<string | undefined>()
  const pre = useRef<HTMLPreElement>(null)
  const from = useRef(0)

  useEffect(() => {
    if (paused || tail.done) return
    let live = true
    let timer: number | undefined
    const poll = async (): Promise<void> => {
      try {
        const range = await api.jobLog(id, from.current)
        if (!live) return
        from.current = range.to
        setError(undefined)
        setTail((held) => fold(held, range))
        if (!range.done) timer = window.setTimeout(() => void poll(), nextDelay(range, CADENCE_MS))
      } catch (failure) {
        if (!live) return
        setError(failure instanceof Error ? failure.message : String(failure))
        timer = window.setTimeout(() => void poll(), CADENCE_MS)
      }
    }
    void poll()
    return () => {
      live = false
      if (timer !== undefined) window.clearTimeout(timer)
    }
  }, [id, paused, tail.done])

  useEffect(() => {
    if (paused || pre.current === null) return
    pre.current.scrollTop = pre.current.scrollHeight
  }, [tail.text, paused])

  return (
    <section className="log-tail">
      <div className="log-tail-head">
        <span className="muted">
          {path === null ? "this job ran in the foreground: no log file" : <code className="mono">{path}</code>}
          {tail.dropped > 0 && ` · ${tail.dropped} earlier byte(s) not shown`}
        </span>
        <span>
          <span className="muted">{tail.done ? "ended" : paused ? "paused" : "live"}</span>{" "}
          {!tail.done && (
            <button type="button" className="copy-ref" onClick={() => setPaused((held) => !held)}>
              {paused ? "resume" : "pause"}
            </button>
          )}
        </span>
      </div>
      {error !== undefined && <p className="error">{error}</p>}
      <pre ref={pre} className="log-tail-text">{tail.text === "" ? <span className="muted">(nothing yet)</span> : tail.text}</pre>
    </section>
  )
}
