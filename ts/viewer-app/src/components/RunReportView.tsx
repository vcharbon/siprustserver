/**
 * A `sip.run-report/1` document, drawn: the headline, where it was read from,
 * each run, then each call with its outcome, its main error and the links to
 * its evidence — failed calls first. A report read off a summary a citation
 * kept says so before anything else: the run's bundle and row are gone, and
 * what stands is what the citation kept.
 */
import type { CompareReport, RunReport } from "@sip/contracts"
import type { JSX } from "react"
import { errorOf, localPath, outcomeOf, repeatsOf, toneOf } from "./CompareReportView.tsx"
import { Link } from "./Link.tsx"

const short = (sha: string | null): string => (sha === null ? "unknown" : sha.slice(0, 10))

/** Failed first, then skipped, then passed; within each, the document's order. */
const rank = (cell: CompareReport.Cell): number => (cell.skipped !== null ? 1 : cell.passed ? 2 : 0)

const Runs = ({ doc }: { doc: RunReport.RunReport }): JSX.Element => (
  <table className="listing report-runs">
    <thead>
      <tr>
        <th>run</th>
        <th>capture</th>
        <th>lane</th>
        <th>kind · status</th>
        <th>env</th>
        <th>code</th>
        <th>started</th>
        <th>cases · failed</th>
        <th>sweep · job</th>
        {doc.source === "summaries" && <th>cited by</th>}
      </tr>
    </thead>
    <tbody>
      {doc.runs.map((run) => (
        <tr key={run.id}>
          <td>run{run.id}</td>
          <td>
            <code>{run.capture}</code>
          </td>
          <td>{run.lane}</td>
          <td>{run.setKind} · {run.status}</td>
          <td>{run.env ?? "unknown"}@{run.branch ?? "unknown"}</td>
          <td>{run.code.length === 0 ? "unknown" : run.code.map((one) => `${one.name} ${short(one.sha)}`).join(", ")}</td>
          <td>{run.startedAt}</td>
          <td>{run.cases} · {run.failed}</td>
          <td>{run.sweepId ?? "—"} · {run.jobId ?? "—"}</td>
          {doc.source === "summaries" && <td>{run.citedBy.length === 0 ? "—" : run.citedBy.join(", ")}</td>}
        </tr>
      ))}
    </tbody>
  </table>
)

export const RunReportView = ({ doc }: { doc: RunReport.RunReport }): JSX.Element => {
  const calls = [...doc.calls].sort((a, b) => rank(a.cell) - rank(b.cell))
  return (
    <div className="run-report">
      {doc.source === "summaries" && (
        <p className="report-summary-note">
          <strong>summary kept by a citation</strong> — run {doc.runs.map((run) => run.id).join(", ")}'s bundle and row
          are gone; what stands is the summary its citation kept: each call's verdict, failed step and main error.
          Its ladders, recording and bless class went with the bundle.
        </p>
      )}
      <p className="report-headline">
        <strong>{doc.headline}</strong>
        {doc.total > doc.runs.length && <span className="muted"> · {doc.runs.length} of {doc.total} runs listed</span>}
      </p>
      <Runs doc={doc} />
      <table className="listing report-calls">
        <thead>
          <tr>
            <th>outcome</th>
            <th>call</th>
            <th>main error</th>
            <th>bless</th>
            <th>evidence</th>
          </tr>
        </thead>
        <tbody>
          {calls.map((call) => (
            <tr key={`${call.capture}/${call.caseId}`}>
              <td>
                <span className={`tag ${toneOf(call.cell)}`}>{outcomeOf(call.cell)}</span>
                {repeatsOf(call.cell) !== "" && <> <span className="tag repeats">{repeatsOf(call.cell)}</span></>}
              </td>
              <td>
                <code>{call.caseId}</code> <span className="muted">run{call.cell.run}</span>
              </td>
              <td>{errorOf(call.cell) === "" ? "—" : <code>{errorOf(call.cell)}</code>}</td>
              <td>
                <span className="tag">{call.cell.bless}</span>
              </td>
              <td>
                <Link to={localPath(call.cell.links.study)}>study</Link>{" "}
                · <a href={localPath(call.cell.links.text)}>text</a>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
