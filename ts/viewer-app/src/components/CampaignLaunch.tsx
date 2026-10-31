/**
 * Campaign runs started from its pages: the whole campaign on the lanes
 * picked (the header's control), or one case or sub-case on one lane or all
 * (the case drawer's), each with the system's data rebuilt first or not. A
 * start answers its job, linked to its page, or the server's refusal.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type JobStarted } from "../api.ts"
import { JobLinks } from "./JobLink.tsx"

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

/** What a campaign start asks beside its lanes: one case and sub-case, or every case. */
export interface Scope {
  readonly case?: string
  readonly subcase?: string
}

type Outcome = { readonly jobs: readonly JobStarted[] } | { readonly error: string } | undefined

const useStart = (capture: string) => {
  const [busy, setBusy] = useState(false)
  const [outcome, setOutcome] = useState<Outcome>(undefined)
  const start = async (lanes: readonly string[], scope: Scope, rebuild: boolean): Promise<void> => {
    setBusy(true)
    setOutcome(undefined)
    try {
      setOutcome(await api.jobStart({ kind: "campaign", capture, lanes, ...scope, rebuild }))
    } catch (error) {
      setOutcome({ error: said(error) })
    } finally {
      setBusy(false)
    }
  }
  return { busy, outcome, start }
}

const Said = ({ outcome }: { readonly outcome: Outcome }): JSX.Element | null =>
  outcome === undefined ? null : "error" in outcome
    ? <span className="cp-nok">run: {outcome.error}</span>
    : <JobLinks jobs={outcome.jobs} />

const Rebuild = ({ on, set }: { readonly on: boolean; readonly set: (on: boolean) => void }): JSX.Element => (
  <label className="cp-check" title="rebuild the system's data from scratch before the run">
    <input type="checkbox" checked={on} onChange={(event) => set(event.target.checked)} />
    rebuild data
  </label>
)

/** The whole campaign on the lanes picked. */
export const CampaignStart = ({ capture, lanes }: { readonly capture: string; readonly lanes: readonly string[] }): JSX.Element => {
  const [picked, setPicked] = useState<readonly string[]>(lanes)
  const [rebuild, setRebuild] = useState(false)
  const { busy, outcome, start } = useStart(capture)
  const toggle = (lane: string) =>
    setPicked((now) => (now.includes(lane) ? now.filter((l) => l !== lane) : [...now, lane]))
  return (
    <details className="cp-launch">
      <summary>start</summary>
      <div className="cp-launch-body">
        {lanes.map((lane) => (
          <label key={lane} className="cp-check mono">
            <input type="checkbox" checked={picked.includes(lane)} onChange={() => toggle(lane)} />
            {lane}
          </label>
        ))}
        <Rebuild on={rebuild} set={setRebuild} />
        <button type="button" disabled={busy || picked.length === 0} onClick={() => void start(picked, {}, rebuild)}>
          run campaign
        </button>
        <Said outcome={outcome} />
      </div>
    </details>
  )
}

/** One case's sub-case, or every sub-case of the case, on one lane or all. */
export const CaseReplay = ({ capture, lanes, scope }: {
  readonly capture: string
  readonly lanes: readonly string[]
  readonly scope: Scope
}): JSX.Element => {
  const [rebuild, setRebuild] = useState(false)
  const [whole, setWhole] = useState(false)
  const { busy, outcome, start } = useStart(capture)
  const asked: Scope = whole && scope.case !== undefined ? { case: scope.case } : scope
  return (
    <div className="cp-replay">
      <b>Replay</b>
      {lanes.map((lane) => (
        <button key={lane} type="button" className="mono" disabled={busy} onClick={() => void start([lane], asked, rebuild)}>
          {lane}
        </button>
      ))}
      {lanes.length > 1 && (
        <button type="button" disabled={busy} onClick={() => void start(lanes, asked, rebuild)}>every lane</button>
      )}
      {scope.subcase !== undefined && (
        <label className="cp-check" title="every sub-case of the case">
          <input type="checkbox" checked={whole} onChange={(event) => setWhole(event.target.checked)} />
          whole case
        </label>
      )}
      <Rebuild on={rebuild} set={setRebuild} />
      <Said outcome={outcome} />
    </div>
  )
}
