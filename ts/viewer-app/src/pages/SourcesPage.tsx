/**
 * The source list (§3 S5–S6): every capture the population holds, its
 * ingest and its standing cut each `current` or `stale`, its counts, its
 * longest call, and its standing per lane — served paged and counted, the
 * stale filter and the name search in the URL so a slice is a link. A
 * capture whose ingest failed is a row with its failure.
 *
 * Two verbs live here: ADD BY PATH (§3 S1) — paths under the configured
 * roots, a set or one capture each, cut or not — which starts an ingest job,
 * and RETIRE (§3 S4), which reads the capture's plan, confirms once on the
 * counts, and starts a retire job. Both link the job they started.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, type JobStarted, type RetirePlan, type SourceRow, type SourcesPage as Page } from "../api.ts"
import { Age } from "../components/Age.tsx"
import { JobLinks } from "../components/JobLink.tsx"
import { Link } from "../components/Link.tsx"
import { useHeld } from "../held.ts"
import { href, setQuery, useQuery } from "../router.ts"

const DEFAULT_LIMIT = 100

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

/** A value that settles before it is used: typing a name must not put a query per keystroke on the store. */
const useSettled = <A,>(value: A, delay: number): A => {
  const [settled, setSettled] = useState(value)
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(value), delay)
    return () => window.clearTimeout(timer)
  }, [value, delay])
  return settled
}

/** The word for a state, toned: current is quiet, stale asks for work, failed and broke are red. */
const State = ({ word, detail }: { readonly word: string; readonly detail?: string | null }): JSX.Element => (
  <span
    className={`tag ${word === "current" ? "muted" : word === "stale" || word === "never" ? "state-stale" : "error"}`}
    title={detail ?? undefined}
  >
    {word}
  </span>
)

/** The lanes' standings of one capture: `lane=status(runN[, f failed])`, as the CLI prints them. */
const Standings = ({ row }: { readonly row: SourceRow }): JSX.Element => (
  <>
    {row.standings.map((one) => (
      <div key={one.lane} className="mono">
        <Link to={href.triageStudy(row.capture, { lane: one.lane })}>{one.lane}</Link>={one.status}(run{one.runId}
        {one.failed > 0 && `, ${one.failed} failed`})
      </div>
    ))}
  </>
)

/** The add-by-path form: paths under the roots, one per line. */
const AddByPath = ({ roots }: { readonly roots: readonly string[] }): JSX.Element => {
  const [text, setText] = useState("")
  const [asSet, setAsSet] = useState(false)
  const [cut, setCut] = useState(true)
  const [busy, setBusy] = useState(false)
  const [outcome, setOutcome] = useState<{ readonly jobs: readonly JobStarted[] } | { readonly error: string } | undefined>()

  const paths = text.split("\n").map((line) => line.trim()).filter((line) => line !== "")

  const start = async (): Promise<void> => {
    setBusy(true)
    setOutcome(undefined)
    try {
      setOutcome(await api.jobStart({ kind: "ingest", paths, set: asSet, cut }))
    } catch (error) {
      setOutcome({ error: said(error) })
    } finally {
      setBusy(false)
    }
  }

  return (
    <details className="add-by-path">
      <summary>add by path</summary>
      <p className="muted">
        A file, a directory, or the files of a ring, under one of the roots:{" "}
        {roots.map((root) => <code key={root} className="mono">{root} </code>)}
        — one path per line; a relative one resolves against the repo.
      </p>
      <textarea
        rows={3}
        value={text}
        placeholder="../SamplePcapGz/OTS_pcaps_vhkh_outgoing/A103-21_145407_sip_vHKH.pcapng.gz"
        onChange={(event) => setText(event.target.value)}
      />
      <div className="request-row">
        <label>
          <input type="checkbox" checked={asSet} onChange={(event) => setAsSet(event.target.checked)} />{" "}
          the paths are ONE capture (consecutive files of one tap)
        </label>
        <label>
          <input type="checkbox" checked={cut} onChange={(event) => setCut(event.target.checked)} /> cut once ingested
        </label>
        <button type="button" className="copy-ref" disabled={busy || paths.length === 0} onClick={() => void start()}>
          {busy ? "starting…" : `add ${paths.length} path(s)`}
        </button>
        {outcome !== undefined && "jobs" in outcome && <JobLinks jobs={outcome.jobs} />}
        {outcome !== undefined && "error" in outcome && <span className="error">{outcome.error}</span>}
      </div>
    </details>
  )
}

/** The retire button: the plan's counts in one confirmation, then the job. */
const RetireButton = ({ capture, onStarted }: { readonly capture: string; readonly onStarted: () => void }): JSX.Element => {
  const [phase, setPhase] = useState<
    | { readonly kind: "idle" }
    | { readonly kind: "planning" }
    | { readonly kind: "confirm"; readonly plan: RetirePlan }
    | { readonly kind: "started"; readonly jobs: readonly JobStarted[] }
    | { readonly kind: "error"; readonly detail: string }
  >({ kind: "idle" })

  const plan = async (): Promise<void> => {
    setPhase({ kind: "planning" })
    try {
      setPhase({ kind: "confirm", plan: await api.retirePlan(capture) })
    } catch (error) {
      setPhase({ kind: "error", detail: said(error) })
    }
  }

  const retire = async (): Promise<void> => {
    try {
      const { jobs } = await api.jobStart({ kind: "retire", captures: [capture] })
      setPhase({ kind: "started", jobs })
      onStarted()
    } catch (error) {
      setPhase({ kind: "error", detail: said(error) })
    }
  }

  if (phase.kind === "confirm") {
    const { plan: shown } = phase
    const rows = Object.entries(shown.rows).filter(([, count]) => count > 0)
    return (
      <span className="retire-confirm">
        <span className="mono">
          {rows.length === 0 ? "no row" : rows.map(([table, count]) => `${table} ${count}`).join(", ")}; {shown.runs} run(s),{" "}
          {shown.bundles} bundle(s) on disk
          {shown.artifactDir !== null && ", the artifact directory"}
          {shown.sessionDir !== null && ", the session directory"}
          {shown.uploads.length > 0 && `, ${shown.uploads.length} upload(s)`}
        </span>
        {shown.holders.length > 0
          ? <span className="error"> held by {shown.holders.map((one) => `job ${one.jobId} (${one.how})`).join(", ")}</span>
          : (
            <button type="button" className="copy-ref danger" onClick={() => void retire()}>
              retire {capture} — remove all of it
            </button>
          )}
        <button type="button" className="copy-ref" onClick={() => setPhase({ kind: "idle" })}>keep</button>
      </span>
    )
  }
  return (
    <span className="rerun">
      <button
        type="button"
        className="copy-ref"
        disabled={phase.kind === "planning" || phase.kind === "started"}
        title="retire the capture: its population row, cut, runs, bundles and artifacts — after one confirmation on the counts"
        onClick={() => void plan()}
      >
        {phase.kind === "planning" ? "counting…" : phase.kind === "started" ? "retiring" : "retire…"}
      </button>
      {phase.kind === "started" && <> <JobLinks jobs={phase.jobs} /></>}
      {phase.kind === "error" && <span className="error"> {phase.detail}</span>}
    </span>
  )
}

const Table = ({ page, onStarted }: { readonly page: Page; readonly onStarted: () => void }): JSX.Element => (
  <table className="listing sources-listing">
    <thead>
      <tr>
        <th>capture</th>
        <th>ingested</th>
        <th>ingest</th>
        <th>prep</th>
        <th>cut</th>
        <th>cases</th>
        <th>refusals</th>
        <th>calls</th>
        <th>span s</th>
        <th>standing per lane</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {page.rows.map((row) => (
        <tr key={row.capture}>
          <td className="mono break">
            <Link to={href.triageStudy(row.capture)}>{row.capture}</Link>
            {row.sourcePath !== null && <div className="muted path">{row.sourcePath}</div>}
          </td>
          <td className="muted">{row.ingestedAt === null ? "never" : <Age at={row.ingestedAt} />}</td>
          <td>
            <State word={row.ingest} detail={row.ingestFailed} />
            {row.ingestFailed !== null && <div className="error">{row.ingestFailed}</div>}
          </td>
          <td className="mono">{row.prepId === null ? "—" : `prep${row.prepId}`}</td>
          <td>
            <State word={row.cut} detail={row.cutBroke} />
            {row.cutBroke !== null && <div className="error">{row.cutBroke}</div>}
          </td>
          <td className="mono">{row.cases}</td>
          <td className="mono">{row.refusals}</td>
          <td className="mono">{row.cases + row.refusals}</td>
          <td className="mono">{row.spanSeconds === null ? "—" : row.spanSeconds.toFixed(1)}</td>
          <td><Standings row={row} /></td>
          <td className="row-actions"><RetireButton capture={row.capture} onStarted={onStarted} /></td>
        </tr>
      ))}
    </tbody>
  </table>
)

export const SourcesPage = (): JSX.Element => {
  const initial = useQuery()
  const [stale, setStale] = useState(() => initial.get("stale") === "1")
  const [needle, setNeedle] = useState(() => initial.get("q") ?? "")
  const [limit, setLimit] = useState(() => initial.get("limit") ?? "")
  const [offset, setOffset] = useState(() => Number(initial.get("offset") ?? "0") || 0)
  const [refresh, setRefresh] = useState(0)
  const settled = useSettled(needle, 250)

  // The filter IS the URL: this slice of the list is a link like any other view.
  useEffect(() => {
    setQuery({ stale: stale ? "1" : "", q: needle, limit, offset: offset === 0 ? "" : String(offset) })
  }, [stale, needle, limit, offset])

  const perPage = limit === "" ? DEFAULT_LIMIT : Math.max(1, Number(limit) || DEFAULT_LIMIT)
  const page = useHeld(
    () => api.sources({ stale, q: settled, limit: perPage, offset }),
    `sources#${stale}#${settled}#${perPage}#${offset}#${refresh}`
  )
  const onStarted = (): void => setRefresh((n) => n + 1)

  const value = page.current
  if (value === undefined) {
    if (page.error !== undefined) return <p className="page error">{page.error}</p>
    return <p className="page muted">loading the source list…</p>
  }
  const last = Math.min(value.total, value.offset + value.rows.length)

  return (
    <div className="page sources-page">
      <header className="page-head">
        <div>
          <h1>sources</h1>
          {page.loading && <div className="counts"><span className="tag">reloading…</span></div>}
          {page.error !== undefined && <p className="error">{page.error}</p>}
        </div>
        <div className="page-actions queue-filters">
          <label>
            <input
              type="checkbox"
              checked={stale}
              onChange={(event) => {
                setStale(event.target.checked)
                setOffset(0)
              }}
            />{" "}
            stale only — needs ingest or needs cut
          </label>
          <label>
            capture{" "}
            <input
              type="search"
              value={needle}
              placeholder="name contains…"
              onChange={(event) => {
                setNeedle(event.target.value)
                setOffset(0)
              }}
            />
          </label>
          <label>
            per page{" "}
            <input
              type="number"
              min={1}
              className="row-cap"
              value={limit}
              placeholder={String(DEFAULT_LIMIT)}
              onChange={(event) => setLimit(event.target.value)}
            />
          </label>
        </div>
      </header>

      <p className="muted">
        showing <b>{value.rows.length === 0 ? 0 : value.offset + 1}–{last}</b> of <b>{value.total}</b> capture(s)
        {" · "}
        <button type="button" className="copy-ref" disabled={value.offset === 0} onClick={() => setOffset(Math.max(0, offset - perPage))}>
          previous
        </button>{" "}
        <button type="button" className="copy-ref" disabled={last >= value.total} onClick={() => setOffset(offset + perPage)}>
          next
        </button>
      </p>

      <AddByPath roots={value.roots} />

      <Table page={value} onStarted={onStarted} />
    </div>
  )
}
