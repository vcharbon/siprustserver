/**
 * Drop captures on the viewer: a capture is saved into the artifact store and
 * handed to the source command, which makes an INGEST JOB — ingest, into the
 * population, cut — and links it here. A replay is a request to create once
 * the job is done: the lane picked is said back, nothing is replayed by the
 * drop itself (§3 S3).
 *
 * Several files dropped together are ONE capture by default — the consecutive
 * files of one tap, a call starting in one and ending in the next — and the
 * ingest refuses them when they are not. The "one capture each" choice posts
 * them one by one instead. Drops are posted one after another so the store
 * sees them in order; each states its own outcome, links its job and its
 * study view, which is the point of dropping it.
 */
import type { JSX } from "react"
import { useEffect, useRef, useState } from "react"
import { api, type CaptureIngestStart } from "../api.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/** One dropped capture, from the queue to its job. */
interface Item {
  readonly id: number
  /** Its file names, in ring order for a set. */
  readonly name: string
  readonly status: "waiting" | "uploading" | "started" | "refused" | "error"
  /** The capture the pipeline named it; known once the drop is accepted. */
  readonly capture?: string
  /** The lane it was posted on; "" = added to the population only. */
  readonly lane?: string
  readonly detail?: string
  /** The ingest job the source command made. */
  readonly jobId?: number
}

const said = (error: unknown): string => error instanceof Error ? error.message : String(error)

/** Name order with numbers compared by value, as the campaign orders a set: `ring9` before `ring10`. */
const natural = new Intl.Collator("en", { numeric: true })

const ringOrder = (files: readonly File[]): readonly File[] =>
  [...files].sort((a, b) => natural.compare(a.name, b.name))

const WORD: Readonly<Record<Item["status"], string>> = {
  waiting: "waiting for the drop before it",
  uploading: "uploading…",
  started: "",
  refused: "refused",
  error: "error"
}

export const DropCaptures = (
  { lane, lanes, onFresh }: {
    /** The lane the page reports on: what a drop replays on unless changed. */
    readonly lane?: string | undefined
    /** The lanes offered beside "add only"; empty = add only is the only choice. */
    readonly lanes: readonly string[]
    /** Called once per capture that lands, so the page behind can reload. */
    readonly onFresh?: () => void
  }
): JSX.Element => {
  const [choice, setChoice] = useState(lane ?? "")
  const [asSet, setAsSet] = useState(true)
  const [items, setItems] = useState<readonly Item[]>([])
  const [over, setOver] = useState(false)

  // Set on every mount: an effect that only clears the flag latches the
  // component dead the moment React remounts it, and the list then sits on
  // "uploading…" for a drop that landed.
  const live = useRef(true)
  useEffect(() => {
    live.current = true
    return () => {
      live.current = false
    }
  }, [])

  const pending = useRef<{ readonly id: number; readonly files: readonly File[] }[]>([])
  const draining = useRef(false)
  const counter = useRef(0)
  // The lane in force when a file is POSTED, not when it was dropped: a lane
  // picked while a queue drains applies to what has not gone yet.
  const laneNow = useRef(choice)
  laneNow.current = choice

  const patch = (id: number, fields: Partial<Item>): void =>
    setItems((all) => all.map((item) => (item.id === id ? { ...item, ...fields } : item)))

  const runOne = async (id: number, files: readonly File[]): Promise<void> => {
    const on = laneNow.current
    patch(id, { status: "uploading", lane: on })
    try {
      const started: CaptureIngestStart = await api.captureIngest(files, on)
      if (!live.current) return
      patch(id, { capture: started.capture, detail: started.detail })
      if (!started.started) {
        patch(id, { status: "refused" })
        return
      }
      patch(id, { status: "started", ...(started.jobId == null ? {} : { jobId: started.jobId }) })
      onFresh?.()
    } catch (error) {
      if (live.current) patch(id, { status: "error", detail: said(error) })
    }
  }

  const drain = async (): Promise<void> => {
    if (draining.current) return
    draining.current = true
    try {
      for (;;) {
        const head = pending.current.shift()
        if (head === undefined) return
        await runOne(head.id, head.files)
      }
    } finally {
      draining.current = false
    }
  }

  const take = (dropped: FileList | null): void => {
    if (dropped === null || dropped.length === 0) return
    const files = ringOrder(Array.from(dropped))
    const captures = asSet ? [files] : files.map((file) => [file])
    const taken = captures.map((group) => ({ id: counter.current++, files: group }))
    setItems((all) => [
      ...all,
      ...taken.map(({ files, id }) => ({
        id,
        name: files.map((file) => file.name).join(" + "),
        status: "waiting" as const
      }))
    ])
    pending.current.push(...taken)
    void drain()
  }

  return (
    <section className="drop-captures">
      <div
        className={`drop-zone${over ? " over" : ""}`}
        onDragOver={(event) => {
          event.preventDefault()
          setOver(true)
        }}
        onDragLeave={() => setOver(false)}
        onDrop={(event) => {
          event.preventDefault()
          setOver(false)
          take(event.dataTransfer.files)
        }}
      >
        <span>
          <b>drop captures here</b>
          <span className="muted"> — .pcap, .pcap.gz, .pcapng, .pcapng.gz</span>
        </span>
        <label className="copy-ref">
          pick files…
          <input
            type="file"
            multiple
            accept=".pcap,.pcapng,.gz"
            hidden
            onChange={(event) => {
              take(event.target.files)
              // Cleared so dropping the same file twice is two drops.
              event.target.value = ""
            }}
          />
        </label>
        <label>
          lane{" "}
          <select value={choice} onChange={(event) => setChoice(event.target.value)}>
            <option value="">add only — ingest and cut, no replay</option>
            {lanes.map((one) => <option key={one} value={one}>replay on {one}</option>)}
          </select>
        </label>
        <label>
          several files are{" "}
          <select
            value={asSet ? "set" : "each"}
            onChange={(event) => setAsSet(event.target.value === "set")}
          >
            <option value="set">one capture — consecutive files of one tap</option>
            <option value="each">one capture each</option>
          </select>
        </label>
      </div>

      {items.length > 0 && (
        <ul className="drop-list">
          {items.map((item) => (
            <li key={item.id} className={`drop-item drop-${item.status}`}>
              <span className="mono break">{item.name}</span>{" "}
              {item.status === "started"
                ? (
                  <>
                    {item.jobId !== undefined
                      ? <Link to={href.job(item.jobId)} className="job-link">job {item.jobId}</Link>
                      : <span className="tag class-accepted">launched</span>}{" "}
                    {item.capture !== undefined && (
                      <Link
                        to={href.triageStudy(item.capture, {
                          lane: item.lane === "" ? undefined : item.lane
                        })}
                      >
                        study {item.capture}
                      </Link>
                    )}
                  </>
                )
                : <span className="muted">{WORD[item.status]}</span>}
              {item.detail !== undefined && item.detail !== "" && (
                <span className="muted"> — {item.detail}</span>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}
