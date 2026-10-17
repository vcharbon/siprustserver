/**
 * The replay control of the queue (§5 R1, R2, R6, R9): the filter bar as it
 * stands on the page's lane (read there whatever lanes are picked), the
 * ticked rows, those rows' captures whole, or the population under its
 * predicates — previewed on the lanes picked (one server call, no
 * row cap: the count and the captures, each full or cases),
 * then created as one request PER LANE with the knobs typed — each lane
 * filling the untyped ones with its own defaults, shown beside the ceilings it
 * clamps them to — and a replay job over each — with a cut
 * queued ahead where "re-cut first" says so. The jobs are linked as they start.
 *
 * Nothing is written until Create: a preview is the server reading the
 * selection the way the resolution would.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type JobStarted, type LanesView, type RequestPreview, type TriageCallRow, useAsync } from "../api.ts"
import {
  createLine,
  describeSelection,
  type KnobsInput,
  laneKnobs,
  laneNotes,
  type Mode,
  NO_KNOBS,
  NO_PREDICATES,
  type Predicates,
  selectionOf,
  statedKnobs
} from "../request-builder.ts"
import { useCommands } from "../commands.ts"
import type { ChipFilter } from "../queue-filter.ts"
import { JobLinks } from "./JobLink.tsx"
import { RequestPreviewPane } from "./RequestPreview.tsx"

type Recut = "none" | "stale-only" | "force"

type Outcome =
  | { readonly kind: "idle" }
  | { readonly kind: "busy"; readonly what: string }
  | { readonly kind: "previewed"; readonly preview: RequestPreview }
  | { readonly kind: "created"; readonly jobs: readonly JobStarted[]; readonly requests: readonly string[] }
  | { readonly kind: "refused"; readonly detail: string }

const MODES: ReadonlyArray<readonly [Mode, string]> = [
  ["filter", "the current filter"],
  ["ticked", "the ticked rows"],
  ["captures", "these captures whole"],
  ["population", "the population"]
]

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

export const RequestBuilder = (
  { lane, lanes: known, scope, q, filter, ticked, shown }: {
    /** The lane the queue is read on: what a filter selection is read on, and the lane picked by default. */
    readonly lane: string
    /** The lanes the queue offers, for the lane picker where the lanes route lists none. */
    readonly lanes: readonly string[]
    readonly scope: string
    readonly q: string
    readonly filter: ChipFilter
    readonly ticked: readonly TriageCallRow[]
    readonly shown: readonly TriageCallRow[]
  }
): JSX.Element => {
  const [open, setOpen] = useState(false)
  const [mode, setMode] = useState<Mode>("filter")
  const [predicates, setPredicates] = useState<Predicates>(NO_PREDICATES)
  const [picked, setPicked] = useState<readonly string[]>(lane === "" ? [] : [lane])
  const [knobs, setKnobs] = useState<KnobsInput>(NO_KNOBS)
  const [recut, setRecut] = useState<Recut>("none")
  const [outcome, setOutcome] = useState<Outcome>({ kind: "idle" })
  const presets = useAsync(() => api.lanes(), "lanes")
  const commands = useCommands()

  const lanesView: LanesView | undefined = presets.state === "ok" ? presets.value : undefined
  const laneNames = lanesView === undefined ? known : lanesView.lanes.map((one) => one.lane)
  const selection = selectionOf(mode, { lane, scope, q, filter }, ticked, shown, predicates)
  const defaults = lanesView?.knobs ?? { concurrency: 8, batchSize: 1000, memoryMax: "4G", startsPerSecond: 0, cleanup: true }
  // One lane picked: its own defaults stand in the empty fields; several: the generic ones, each lane's noted below.
  const only = picked.length === 1 ? lanesView?.lanes.find((one) => one.lane === picked[0]) : undefined
  const placeholders = only === undefined ? defaults : laneKnobs(NO_KNOBS, only, defaults)
  const previewed = outcome.kind === "previewed" ? outcome.preview : undefined

  const body = () => (typeof selection === "string" ? undefined : { lanes: picked, selection })

  const preview = async (): Promise<void> => {
    const request = body()
    if (request === undefined) return
    setOutcome({ kind: "busy", what: "previewing…" })
    try {
      const shown = await api.requestPreview(request)
      setOutcome({ kind: "previewed", preview: shown })
    } catch (error) {
      setOutcome({ kind: "refused", detail: said(error) })
    }
  }

  const create = async (): Promise<void> => {
    const request = body()
    if (request === undefined) return
    setOutcome({ kind: "busy", what: "creating…" })
    try {
      const { created } = await api.requestCreate({ ...request, knobs: statedKnobs(knobs) })
      const jobs: Array<JobStarted> = []
      for (const one of created) {
        const started = await api.jobStart({
          kind: "replay",
          requestId: one.requestId,
          ...(recut === "none" ? {} : { recutFirst: recut })
        })
        jobs.push(...started.jobs)
      }
      setOutcome({
        kind: "created",
        jobs,
        requests: created.map((one) => `request ${one.requestId} on ${one.lane}: ${one.cells} cell(s)`)
      })
    } catch (error) {
      setOutcome({ kind: "refused", detail: said(error) })
    }
  }

  const toggleLane = (name: string): void =>
    setPicked((held) => (held.includes(name) ? held.filter((one) => one !== name) : [...held, name]))

  const knob = (key: keyof Omit<KnobsInput, "cleanup">, label: string, placeholder: string): JSX.Element => (
    <label key={key}>
      {label}{" "}
      <input
        type="text"
        className="row-cap"
        value={knobs[key]}
        placeholder={placeholder}
        onChange={(event) => setKnobs((held) => ({ ...held, [key]: event.target.value }))}
      />
    </label>
  )

  if (!open) {
    return (
      <div className="request-builder collapsed">
        <button type="button" className="copy-ref" onClick={() => setOpen(true)}>
          replay… {ticked.length > 0 && <span className="count">{ticked.length} ticked</span>}
        </button>
      </div>
    )
  }

  const ready = typeof selection !== "string" && picked.length > 0 && outcome.kind !== "busy"

  return (
    <section className="request-builder">
      <header className="request-head">
        <b>replay</b>
        <button type="button" className="copy-ref" onClick={() => setOpen(false)}>close</button>
      </header>

      <div className="request-row">
        <span className="facet-label">selection</span>
        {MODES.map(([value, label]) => (
          <label key={value}>
            <input type="radio" name="request-mode" checked={mode === value} onChange={() => setMode(value)} />{" "}
            {label}
            {value === "ticked" && ` (${ticked.length})`}
          </label>
        ))}
      </div>

      {mode === "population" && (
        <div className="request-row">
          <label>
            <select
              value={predicates.universe}
              onChange={(event) => setPredicates({ ...predicates, universe: event.target.value === "all" ? "all" : "standing" })}
            >
              <option value="standing">standing (the population)</option>
              <option value="all">all (the universe)</option>
            </select>
          </label>
          <label>
            max-span s{" "}
            <input type="text" className="row-cap" value={predicates.maxSpan} onChange={(e) => setPredicates({ ...predicates, maxSpan: e.target.value })} />
          </label>
          <label>
            green-on{" "}
            <input type="text" value={predicates.greenOn} placeholder="lane" onChange={(e) => setPredicates({ ...predicates, greenOn: e.target.value })} />
          </label>
          <label>
            sample{" "}
            <input type="text" className="row-cap" value={predicates.sample} onChange={(e) => setPredicates({ ...predicates, sample: e.target.value })} />
          </label>
          <label>
            limit{" "}
            <input type="text" className="row-cap" value={predicates.limit} onChange={(e) => setPredicates({ ...predicates, limit: e.target.value })} />
          </label>
        </div>
      )}

      <p className="muted mono request-line">
        {typeof selection === "string"
          ? selection
          : commands.request === undefined
          ? describeSelection(selection)
          : createLine(commands.request, picked, selection)}
      </p>

      <div className="request-row">
        <span className="facet-label">lanes</span>
        {laneNames.map((name) => (
          <label key={name}>
            <input type="checkbox" checked={picked.includes(name)} onChange={() => toggleLane(name)} /> {name}
          </label>
        ))}
        <span className="muted">one request per lane</span>
      </div>

      <div className="request-row">
        <span className="facet-label">knobs</span>
        {knob("concurrency", "concurrency", String(placeholders.concurrency))}
        {knob("batchSize", "batch (cells)", String(placeholders.batchSize))}
        {knob("memoryMax", "memory max", placeholders.memoryMax)}
        {knob("startsPerSecond", "starts/s", String(placeholders.startsPerSecond))}
        <label>
          <input type="checkbox" checked={knobs.cleanup} onChange={(e) => setKnobs({ ...knobs, cleanup: e.target.checked })} />{" "}
          cleanup after
        </label>
        <label>
          re-cut first{" "}
          <select value={recut} onChange={(event) => setRecut(event.target.value as Recut)}>
            <option value="none">no</option>
            <option value="stale-only">stale only</option>
            <option value="force">force</option>
          </select>
        </label>
      </div>
      {lanesView !== undefined && (
        <ul className="request-notes muted">
          {lanesView.lanes
            .filter((one) => picked.includes(one.lane))
            .flatMap((one) => laneNotes(knobs, one, defaults))
            .map((note) => <li key={note}>{note}</li>)}
        </ul>
      )}

      <div className="request-row">
        <button type="button" className="copy-ref" disabled={!ready} onClick={() => void preview()}>preview</button>
        <button type="button" className="copy-ref" disabled={!ready} onClick={() => void create()}>
          create {picked.length > 1 ? `${picked.length} requests` : "request"} and start
        </button>
        {outcome.kind === "busy" && <span className="muted">{outcome.what}</span>}
        {outcome.kind === "refused" && <span className="error">{outcome.detail}</span>}
      </div>

      {previewed !== undefined && (
        <RequestPreviewPane preview={previewed} />
      )}

      {outcome.kind === "created" && (
        <div className="request-created">
          {outcome.requests.map((line) => <div key={line} className="mono">{line}</div>)}
          <JobLinks jobs={outcome.jobs} />
        </div>
      )}
    </section>
  )
}
