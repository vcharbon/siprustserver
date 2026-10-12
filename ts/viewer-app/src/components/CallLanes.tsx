/**
 * One call on EVERY lane: its features, its latest result per lane with the
 * link to its study there and a button replaying it there, and its recent
 * runs whatever the lane, newest first. A replay started here lands on the
 * next load of the page.
 */
import type { JSX } from "react"
import { api, useAsync, type TriageCallHistoryEntry } from "../api.ts"
import { href } from "../router.ts"
import { Age } from "./Age.tsx"
import { Link } from "./Link.tsx"
import { ReplayButton } from "./ReplayButton.tsx"


const stateOf = (entry: TriageCallHistoryEntry): { label: string; tone: string } =>
  entry.skipped !== null
    ? { label: "skipped", tone: "muted" }
    : entry.passed
    ? { label: "passed", tone: "matched" }
    : { label: "failed", tone: "missing" }

const Result = ({ entry }: { entry: TriageCallHistoryEntry }): JSX.Element => {
  const state = stateOf(entry)
  return (
    <>
      <span className={`tag ${state.tone}`} title={entry.skipped ?? undefined}>{state.label}</span>
      {entry.errorRule !== null && <span className="mono"> {entry.errorRule}</span>}
      {entry.errorExpected !== null && <span className="muted"> — waited for {entry.errorExpected}</span>}
    </>
  )
}

const RunCell = ({ entry }: { entry: TriageCallHistoryEntry }): JSX.Element => (
  <>
    run{entry.runId} {entry.setKind}
    {entry.jobId !== null && <> · <Link to={href.job(entry.jobId)}>job {entry.jobId}</Link></>}
    {entry.finishedAt !== null && <> · <Age at={entry.finishedAt} /></>}
  </>
)

export const CallLanes = (
  { capture, caseId, lane }: {
    capture: string
    caseId: string
    /** The lane the page reads on: its row is marked, the others link to their own study. */
    lane: string | undefined
  }
): JSX.Element => {
  const history = useAsync(
    () => Promise.all([api.triageCallHistory(capture, caseId), api.lanes()]),
    `${capture}#${caseId}`
  )
  if (history.state === "loading") return <p className="muted">loading this call's history…</p>
  if (history.state === "error") return <p className="error">history: {history.message}</p>
  const [{ latest: runs, recent }, known] = history.value
  const latest = new Map(runs.map((entry) => [entry.lane, entry]))
  const shown = known.lanes.map((one) => one.lane)
  const features = recent[0]?.features ?? []
  return (
    <section className="call-lanes">
      <p className="counts">
        <span className="muted">features:</span>
        {features.length === 0
          ? <span className="muted">none recorded</span>
          : features.map((feature) => (
            <Link key={feature} to={href.triage({ lane, scope: "all", feature })} className="tag">{feature}</Link>
          ))}
      </p>
      <table className="listing call-lanes-table">
        <thead>
          <tr>
            <th>lane</th>
            <th>latest</th>
            <th>run</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {shown.map((one) => {
            const entry = latest.get(one)
            return (
              <tr key={one} className={one === lane ? "current-lane" : undefined}>
                <td className="mono">
                  {one === lane
                    ? <b>{one}</b>
                    : <Link to={href.triageStudy(capture, { lane: one, case: caseId })}>{one}</Link>}
                </td>
                <td>{entry === undefined ? <span className="muted">never run</span> : <Result entry={entry} />}</td>
                <td className="muted mono">{entry !== undefined && <RunCell entry={entry} />}</td>
                <td className="row-actions">
                  <ReplayButton capture={capture} lane={one} caseId={caseId} />
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
      {recent.length > 0 && (
        <details>
          <summary className="muted">recent runs, every lane ({recent.length})</summary>
          <table className="listing call-lanes-table">
            <tbody>
              {recent.map((entry) => (
                <tr key={entry.runId}>
                  <td className="mono">{entry.lane}</td>
                  <td><Result entry={entry} /></td>
                  <td className="muted mono"><RunCell entry={entry} /></td>
                </tr>
              ))}
            </tbody>
          </table>
        </details>
      )}
    </section>
  )
}
