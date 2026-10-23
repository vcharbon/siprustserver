/**
 * One job (§6 J3, J4, J8): its status, exit and instants, the request it
 * runs with its selection, the chain it restarts, the progress bar with its
 * counts and the batch it is on, the live log, the hook logs folded, the
 * cells the resolution found gone with their reasons, and the cells landed
 * red so far, each linked to its study — and the same verbs as the list.
 * Re-read every two seconds while the job runs; everything shown is a row
 * or a file, so a viewer restart changes nothing here.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, type JobCells, type JobDetail, type JobHook, type JobRedCell, type TriageMainError } from "../api.ts"
import { Age } from "../components/Age.tsx"
import { JobActions } from "../components/JobActions.tsx"
import { Link } from "../components/Link.tsx"
import { LogTail } from "../components/LogTail.tsx"
import { ProgressBar } from "../components/ProgressBar.tsx"
import { useHeld } from "../held.ts"
import { unitOf } from "../job-progress.ts"
import { alsoBadBody } from "../bad-body.ts"
import { sourceFaultText } from "../source-fault.ts"
import { usePolled } from "../poll.ts"
import { href } from "../router.ts"

const POLL_MS = 2_000
const CELLS_SHOWN = 200

const ENDED = new Set(["done", "failed", "interrupted", "died"])

/**
 * The main error in one line: the rule and its citation, where a died call
 * stopped, the failure's tag; then the source fault beside it, where one is.
 */
const errorText = (error: TriageMainError | null): string => {
  const source = error?.source
  return mainText(error) + (source === undefined ? "" : ` + ${sourceFaultText(source)}`)
}

const mainText = (error: TriageMainError | null): string => {
  if (error === null) return "not measured"
  switch (error.kind) {
    case "rfc":
      return `${error.rule ?? "rfc"}${error.rfc === null ? "" : ` · ${error.rfc}`}`
    case "died":
      return `died${error.step === null ? "" : ` at ${error.step}`}${error.expected === null ? "" : ` · expected ${error.expected}`}`
    case "failure":
      return `${error.rule ?? "failed"}${error.step === null ? "" : ` at ${error.step}`}`
    case "bad-body":
      return `bad body${error.step === null ? "" : ` at ${error.step}`}`
    case "refused":
      return error.rule ?? "refused"
  }
}

/** `capture-case1` under `capture.pcap.gz` reads as `…-case1`. */
const shortCase = (cell: JobRedCell): string => cell.caseId.replace(cell.capture, "…")

const Kin = ({ kin, word }: { readonly kin: readonly JobDetail["lineage"]["ancestors"][number][]; readonly word: string }): JSX.Element | null =>
  kin.length === 0 ? null : (
    <span>
      {word}{" "}
      {kin.map((one, i) => (
        <span key={one.id}>
          {i > 0 && ", "}
          <Link to={href.job(one.id)}>job {one.id}</Link> <span className="muted">({one.mode ?? "first"}, {one.status})</span>
        </span>
      ))}
    </span>
  )

const GoneTable = ({ slice }: { readonly slice: Extract<JobCells, { outcome: "gone" }> }): JSX.Element => (
  <table className="listing jobs-cells">
    <thead>
      <tr>
        <th>capture</th>
        <th>call key</th>
        <th>outcome</th>
        <th>reasons</th>
      </tr>
    </thead>
    <tbody>
      {slice.cells.map((cell) => (
        <tr key={`${cell.capture}/${cell.callKey}`}>
          <td className="mono break"><Link to={href.triageStudy(cell.capture)}>{cell.capture}</Link></td>
          <td className="mono break">{cell.callKey}</td>
          <td><span className="tag muted">{cell.outcome}</span></td>
          <td>{cell.reasons.map((reason) => <span key={reason} className="tag error-refused">{reason}</span>)}</td>
        </tr>
      ))}
    </tbody>
  </table>
)

const RedTable = ({ slice }: { readonly slice: Extract<JobCells, { outcome: "red" }> }): JSX.Element => (
  <table className="listing jobs-cells">
    <thead>
      <tr>
        <th>capture</th>
        <th>call</th>
        <th>run</th>
        <th>main error</th>
      </tr>
    </thead>
    <tbody>
      {slice.cells.map((cell) => (
        <tr key={`${cell.runId}/${cell.caseId}`} className="call-failed">
          <td className="mono break"><Link to={href.triageStudy(cell.capture, { lane: cell.lane })}>{cell.capture}</Link></td>
          <td className="mono break">
            <Link to={href.triageStudy(cell.capture, { lane: cell.lane, case: cell.caseId })}>{shortCase(cell)}</Link>
          </td>
          <td className="mono">run{cell.runId}</td>
          <td>
            <span className={`tag main-error error-${cell.error?.kind ?? "failure"}`} title={cell.error?.detail}>
              {errorText(cell.error)}
            </span>
            {alsoBadBody(cell.error, cell.bodyFailing) && (
              <span className="tag main-error error-bad-body also" title="a body row no rule accepts">
                + bad body
              </span>
            )}
          </td>
        </tr>
      ))}
    </tbody>
  </table>
)

const Hooks = ({ hooks }: { readonly hooks: readonly JobHook[] }): JSX.Element | null =>
  hooks.length === 0 ? null : (
    <section>
      <h2>hooks <span className="muted">({hooks.length})</span></h2>
      {hooks.map((hook) => (
        <details key={hook.name} className="job-hook">
          <summary className="mono">{hook.name}.log <span className="muted">({hook.text.length} chars)</span></summary>
          <pre className="log-tail-text">{hook.text === "" ? <span className="muted">(empty)</span> : hook.text}</pre>
        </details>
      ))}
    </section>
  )

export const JobPage = ({ id }: { readonly id: number }): JSX.Element => {
  const [refresh, setRefresh] = useState(0)
  // Polled every two seconds until the answer says the job has ended.
  const [every, setEvery] = useState<number | null>(POLL_MS)
  const held = usePolled(() => api.job(id), `job#${id}#${refresh}`, every)
  const job = held.current
  const status = job?.status ?? "queued"
  const running = !ENDED.has(status)
  useEffect(() => {
    setEvery(ENDED.has(status) ? null : POLL_MS)
  }, [status])
  // The cells and the hooks follow the landing count while the job runs; once it has ended, one read each.
  const cellsKey = `${id}#${refresh}#${running ? job?.progress.landed ?? 0 : "ended"}`
  const gone = useHeld(() => api.jobCells(id, "gone", CELLS_SHOWN), `gone#${cellsKey}`)
  const red = useHeld(() => api.jobCells(id, "red", CELLS_SHOWN), `red#${cellsKey}`)
  const hooks = useHeld(() => api.jobHooks(id), `hooks#${id}#${refresh}#${status}`)
  const onChanged = (): void => setRefresh((n) => n + 1)

  if (job === undefined) {
    if (held.error !== undefined) return <p className="page error">{held.error}</p>
    return <p className="page muted">loading job {id}…</p>
  }
  const unit = unitOf(job.kind)

  return (
    <div className="page job-page">
      <header className="page-head">
        <div>
          <h1>
            job {job.id} <span className="muted">— {job.label}</span>{" "}
            <Link to={href.jobs()} className="muted">(all jobs)</Link>
          </h1>
          <p className="job-facts">
            <span className={`tag ${job.status === "done" ? "status-passing" : ENDED.has(job.status) ? "error" : "state-stale"}`}>
              {job.status}
            </span>
            {job.exit !== null && <span className="mono"> exit {job.exit}</span>}
            {job.pid !== null && <span className="muted"> · pid {job.pid}</span>}
            {job.startedAt !== null && <> · started <Age at={job.startedAt} /></>}
            {job.finishedAt !== null && <> · finished <Age at={job.finishedAt} /></>}
            {job.stopRequestedAt !== null && (
              <> · <span className="state-stale">stop requested <Age at={job.stopRequestedAt} /> (grace {job.stopGrace ?? "?"} s)</span></>
            )}
            <span className="muted"> · by {job.createdBy}</span>
          </p>
          <p className="muted">
            <Kin kin={job.lineage.ancestors} word={`${job.mode ?? "restart"} of`} />
            {job.lineage.ancestors.length > 0 && job.lineage.descendants.length > 0 && " · "}
            <Kin kin={job.lineage.descendants} word="restarted by" />
          </p>
          {job.request !== null && (
            <p className="mono">
              request {job.request.id} on <Link to={href.triage({ lane: job.request.lane })}>{job.request.lane}</Link>:{" "}
              {job.request.cells} cell(s) over {job.request.captures} capture(s) — <code>{job.request.selection}</code>
            </p>
          )}
          {held.error !== undefined && <p className="error">{held.error}</p>}
        </div>
        <div className="page-actions">
          <JobActions
            id={job.id}
            kind={job.kind}
            status={job.status}
            requestId={job.request?.id ?? null}
            stopRequestedAt={job.stopRequestedAt}
            onChanged={onChanged}
          />
        </div>
      </header>

      <section className="job-progress">
        <ProgressBar progress={job.progress} unit={unit} startedAt={job.startedAt} finishedAt={job.finishedAt} />
        <p className="muted">
          {job.progress.total} planned, {job.progress.landed} landed, {job.progress.pending} pending,{" "}
          {job.progress.goneRefused + job.progress.goneMissing} gone ({job.progress.goneRefused} refused,{" "}
          {job.progress.goneMissing} not in this cut)
          {job.kind === "replay" && (
            <>
              {" · "}
              {job.progress.passed} passed, {job.progress.failed} failed, {job.progress.skipped} skipped over{" "}
              {job.progress.runs} run(s), {job.progress.openRuns} open
            </>
          )}
          {job.batchAt !== null && job.batches !== null && <> · batch {job.batchAt}/{job.batches}</>}
          {job.waitingOn !== null && <span className="waiting"> · waiting on {job.waitingOn}</span>}
          {job.sweepId !== null && <> · sweep {job.sweepId}</>}
        </p>
      </section>

      <h2>log</h2>
      <LogTail id={job.id} path={job.logPath} />

      {hooks.current !== undefined && <Hooks hooks={hooks.current} />}

      {gone.current !== undefined && gone.current.outcome === "gone" && gone.current.total > 0 && (
        <section>
          <h2>
            gone cells <span className="muted">({gone.current.total}{gone.current.total > gone.current.cells.length && `, first ${gone.current.cells.length}`})</span>
          </h2>
          <GoneTable slice={gone.current} />
        </section>
      )}

      {red.current !== undefined && red.current.outcome === "red" && (
        <section>
          <h2>
            red cells so far{" "}
            <span className="muted">({red.current.total}{red.current.total > red.current.cells.length && `, first ${red.current.cells.length}`})</span>
          </h2>
          {red.current.total === 0
            ? <p className="muted">none landed red{running ? " yet — results are written as each capture's row closes" : ""}</p>
            : <RedTable slice={red.current} />}
        </section>
      )}
    </div>
  )
}
