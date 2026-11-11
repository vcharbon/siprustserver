/**
 * An end-to-end campaign's summary, over its full runs (a partial run only
 * where asked by id), with a control that runs the campaign: per-lane tiles
 * of the run read against the one compared, then the failures as facets (the landing tab) or the run grid;
 * a case's history opens as a drawer from either, and a run's delta of it as a
 * modal. A campaign with no run in the environment read opens on the start
 * of its first run (`CampaignNoRun`). With no campaign named, the campaigns
 * the server lists.
 */
import type { JSX } from "react"
import { useEffect, useMemo, useState } from "react"
import { api, type Campaign, type CampaignLane, type CampaignRun, useAsync } from "../api.ts"
import { type FacetKind, facets as facetsOf, lanesRead, SELECTED } from "../campaign-facets.ts"
import { grid as gridOf } from "../campaign-history.ts"
import { type CampaignState, paramsOf, readState, runOf, summaryOf, vsOf } from "../campaign-state.ts"
import { CampaignFailures } from "../components/CampaignFailures.tsx"
import { CampaignEnvPick } from "../components/CampaignEnvPick.tsx"
import { CampaignStart } from "../components/CampaignLaunch.tsx"
import { CampaignGrid } from "../components/CampaignGrid.tsx"
import { CampaignNoRun } from "../components/CampaignNoRun.tsx"
import { CaseHistory } from "../components/CaseHistory.tsx"
import { CheckDelta } from "../components/CheckDelta.tsx"
import { Link } from "../components/Link.tsx"
import { href, setQuery, useQuery } from "../router.ts"

const count = (run: CampaignRun | undefined, lane: string, outcome: string): number =>
  run?.lanes.find((l) => l.lane === lane)?.results.filter((r) => r.outcome === outcome).length ?? 0

const notRun = (run: CampaignRun | undefined, lane: string): number =>
  run?.lanes.find((l) => l.lane === lane)?.skipped ?? 0

/** What a lane did not run, by status (`2 excluded · 5 not ported`); undefined where it ran every cell. */
const notRunTitle = (lane: CampaignLane): string | undefined => {
  const counts = Object.entries(lane.notRun ?? (lane.excluded.length === 0 ? {} : { excluded: lane.excluded.length }))
  return counts.length === 0 ? undefined : counts.map(([status, n]) => `${n} ${status}`).join(" · ")
}

const runLabel = (run: CampaignRun): string =>
  `#${run.id}${run.scope === null ? "" : "°"} · ${run.startedAt.slice(5, 16).replace("T", " ")}${
    run.labels.filter((l) => l.name !== "branch" && l.name !== "env").slice(0, 3).map((l) => ` · ${l.value}`).join("")
  }${(run.selection?.allow.length ?? 0) === 0 ? "" : ` · allowed ${run.selection!.allow.join(",")}`}${
    run.finishedAt === null ? " · running" : ""
  }`

const Tiles = ({ view, run, vs, lanes }: {
  readonly view: Campaign
  readonly run: CampaignRun | undefined
  readonly vs: CampaignRun | undefined
  readonly lanes: readonly string[]
}): JSX.Element => (
  <div className="cp-tiles">
    {lanes.map((lane) => {
      const failing = count(run, lane, "failure")
      const errors = count(run, lane, "error")
      const passing = count(run, lane, "pass")
      const skipped = notRun(run, lane)
      const was = vs === undefined ? undefined : count(vs, lane, "failure") + count(vs, lane, "error")
      const delta = was === undefined ? undefined : failing + errors - was
      const total = Math.max(1, failing + errors + passing)
      const status = run?.lanes.find((l) => l.lane === lane)
      if (status === undefined) {
        return (
          <div key={lane} className="cp-tile">
            <h3 className="mono">{lane}</h3>
            <div className="muted">— not run</div>
          </div>
        )
      }
      return (
        <div key={lane} className="cp-tile">
          <h3 className="mono">{lane}</h3>
          <div>
            <span className={`cp-big ${failing > 0 ? "cp-nok" : "cp-ok"}`}>{failing}</span> ✗ ·{" "}
            <span className="cp-err">{errors}</span> ! · <span className="cp-ok">{passing}</span> ✓ ·{" "}
            <span className="muted" title={notRunTitle(status)}>{skipped}</span> —
          </div>
          <div className="cp-bar">
            <i className="pass" style={{ width: `${(passing / total) * 100}%` }} />
            <i className="failure" style={{ width: `${(failing / total) * 100}%` }} />
            <i className="error" style={{ width: `${(errors / total) * 100}%` }} />
          </div>
          <div className="muted mono">
            {delta === undefined ? "" : `${delta > 0 ? "+" : delta < 0 ? "" : "±"}${delta} vs #${vs!.id}`}
            {run?.finishedAt === null ? " · running" : status?.status === "error" ? ` · ! ${status.detail ?? ""}` : ""}
          </div>
        </div>
      )
    })}
  </div>
)

const Campaigns = (): JSX.Element => {
  const listed = useAsync(() => api.campaigns(), "campaigns")
  return (
    <div className="page campaign">
      <h1>Campaigns</h1>
      {listed.state === "loading" && <p className="muted">…</p>}
      {listed.state === "error" && <p className="cp-nok">{listed.message}</p>}
      {listed.state === "ok" && (
        <ul>
          {listed.value.map(({ capture, title }) => <li key={capture}><Link to={href.campaign(capture)}>{title ?? capture}</Link></li>)}
          {listed.value.length === 0 && <li className="muted">none</li>}
        </ul>
      )}
    </div>
  )
}

const Summary = ({ view: whole }: { readonly view: Campaign }): JSX.Element => {
  const query = useQuery()
  const [state, setState] = useState<CampaignState>(() => readState(query))
  const view = useMemo(() => summaryOf(whole, state.run), [whole, state.run])
  const update = (patch: Partial<CampaignState>) => setState((now) => ({ ...now, ...patch }))
  useEffect(() => setQuery(paramsOf({ ...state, env: view.env ?? undefined })), [state, view.env])
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return
      setState((now) => (now.delta !== undefined ? { ...now, delta: undefined } : { ...now, caseId: undefined }))
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [])

  const runId = runOf(view, state)
  const vsId = vsOf(view, state, runId)
  const lanes = lanesRead(view, state.filters)
  const facets = useMemo(
    () => (runId === undefined ? undefined : facetsOf(view, runId, vsId, state.filters, state.selection)),
    [view, runId, vsId, state.filters, state.selection]
  )
  const grid = useMemo(() => gridOf(view, lanes), [view, lanes])
  const run = view.runs.find((r) => r.id === runId)
  const vs = view.runs.find((r) => r.id === vsId)

  const toggleLane = (lane: string) => {
    const now = state.filters.lanes.length === 0 ? view.lanes : state.filters.lanes
    const next = now.includes(lane) ? now.filter((l) => l !== lane) : [...now, lane]
    if (next.length === 0) return
    update({ filters: { ...state.filters, lanes: next.length === view.lanes.length ? [] : next } })
  }
  const toggleFacet = (kind: FacetKind, key: string) => {
    const field = SELECTED[kind]
    const list = state.selection[field]
    const next = list.includes(key) ? list.filter((k) => k !== key) : [...list, key]
    update({ tab: "failures", selection: { ...state.selection, [field]: next } })
  }
  const flag = (name: "failing" | "oneLane" | "fresh", label: string, tip: string) => (
    <label className="cp-check" title={tip}>
      <input
        type="checkbox"
        checked={state.filters[name]}
        onChange={(event) => update({ filters: { ...state.filters, [name]: event.target.checked } })}
      />
      {label}
    </label>
  )

  return (
    <div className="page campaign">
      <header className="cp-head">
        <div>
          <h1>{view.title ?? "Campaign summary"}</h1>
          <p className="muted mono">{view.capture}{view.env === null ? "" : ` · ${view.env}`}</p>
        </div>
        <div className="cp-controls">
          <CampaignEnvPick view={view} />
          <label>
            run{" "}
            <select value={runId ?? ""} onChange={(event) => update({ run: Number(event.target.value) })}>
              {view.runs.map((r) => <option key={r.id} value={r.id}>{runLabel(r)}</option>)}
            </select>
          </label>
          <label>
            vs{" "}
            <select value={vsId ?? ""} onChange={(event) => update({ vs: Number(event.target.value) })}>
              {view.runs.filter((r) => r.id !== runId).map((r) => <option key={r.id} value={r.id}>{runLabel(r)}</option>)}
            </select>
          </label>
          <span className="cp-lanebuttons">
            {view.lanes.map((lane) => (
              <button
                key={lane}
                type="button"
                className={`mono${lanes.includes(lane) ? " on" : ""}`}
                onClick={() => toggleLane(lane)}
              >
                {lane}
              </button>
            ))}
          </span>
          {flag("failing", "failing", "fails on a lane read")}
          {flag("oneLane", "one lane", "fails on exactly one lane read")}
          {flag("fresh", "new", "fails where the compared run did not")}
          {view.launch !== null && (
            <CampaignStart
              key={view.capture}
              capture={view.capture}
              lanes={view.launch.lanes}
              stated={view.launch.defaultLanes}
              tags={view.launch.tags}
              cases={view.cases}
            />
          )}
          <input
            className="cp-filter"
            placeholder="filter"
            value={state.filters.text}
            onChange={(event) => update({ filters: { ...state.filters, text: event.target.value } })}
          />
        </div>
      </header>
      {run === undefined ? <p className="muted">no run</p> : (
        <>
          <Tiles view={view} run={run} vs={vs} lanes={lanes} />
          <p className="muted cp-legend">✓ pass · ✗ fail · ! error, unverified · — not run</p>
          <nav className="cp-tabs">
            <button type="button" className={state.tab === "failures" ? "on" : ""} onClick={() => update({ tab: "failures" })}>
              Failures
            </button>
            <button type="button" className={state.tab === "grid" ? "on" : ""} onClick={() => update({ tab: "grid" })}>
              Run grid
            </button>
          </nav>
          {state.tab === "grid"
            ? <CampaignGrid view={view} grid={grid} lanes={lanes} run={run.id} onCase={(caseId) => update({ caseId })} />
            : facets !== undefined && (
              <CampaignFailures
                view={view}
                facets={facets}
                run={run.id}
                vs={vsId}
                lanes={lanes}
                selected={state.selection}
                onToggle={toggleFacet}
                onCase={(caseId) => update({ caseId })}
              />
            )}
        </>
      )}
      {state.caseId !== undefined && (
        <CaseHistory
          view={whole}
          caseId={state.caseId}
          lanes={lanes}
          onClose={() => update({ caseId: undefined, delta: undefined })}
          onDelta={(id, lane) => update({ delta: { run: id, lane } })}
        />
      )}
      {state.caseId !== undefined && state.delta !== undefined && (
        <CheckDelta
          view={whole}
          caseId={state.caseId}
          run={state.delta.run}
          lane={state.delta.lane}
          onClose={() => update({ delta: undefined })}
        />
      )}
    </div>
  )
}

export const CampaignPage = ({ capture }: { readonly capture?: string | undefined }): JSX.Element => {
  const asked = readState(useQuery())
  const loaded = useAsync(
    () =>
      capture === undefined
        ? Promise.resolve(undefined)
        : api.campaign(capture, { env: asked.env, run: asked.delta?.run ?? asked.run }),
    `${capture ?? ""}|${asked.env ?? ""}`
  )
  if (capture === undefined) return <Campaigns />
  if (loaded.state === "loading") return <div className="page campaign"><p className="muted">…</p></div>
  if (loaded.state === "error") return <div className="page campaign"><p className="cp-nok">{loaded.message}</p></div>
  return loaded.value === undefined
    ? <Campaigns />
    : loaded.value.runs.length === 0
    ? <CampaignNoRun view={loaded.value} />
    : <Summary view={loaded.value} />
}
