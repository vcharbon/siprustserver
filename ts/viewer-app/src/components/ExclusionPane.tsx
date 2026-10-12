/**
 * The refusal an excluded entry carries: why the generator wrote no case, on
 * which calls, and what it saw down to the datagram.
 *
 * This pane REPLACES the scenario ladder rather than standing beside one. There
 * is no case here and none is implied: no lanes, no runs, no compare. What a
 * reviewer can still do is read the capture, which is the `source` tab.
 */
import { parseExclusions, type ExclusionRecord } from "@sip/viewer-core"
import type { JSX } from "react"
import type { CaseExclusion } from "../api.ts"

const Evidence = ({ record }: { record: ExclusionRecord }): JSX.Element | null => {
  if (record.evidence.length === 0) return null
  return (
    <table className="listing exclusion-evidence">
      <thead>
        <tr>
          <th>rule</th>
          <th>charged</th>
          <th>where</th>
          <th>call-id</th>
          <th>what was seen</th>
        </tr>
      </thead>
      <tbody>
        {record.evidence.map((entry, index) => (
          <tr key={`${record.reason}-${index}`}>
            <td className="mono">{entry.rule ?? "—"}</td>
            <td className="mono">{entry.emitter ?? "—"}</td>
            <td className="mono">
              {[
                entry.group === undefined ? undefined : `group ${entry.group}`,
                entry.leg === undefined ? undefined : `leg ${entry.leg}`,
                entry.msg === undefined ? undefined : `msg ${entry.msg}`
              ].filter((part) => part !== undefined).join(" · ") || "—"}
            </td>
            <td className="mono break">{entry.callId ?? "—"}</td>
            <td>{entry.detail}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

const Record = ({ record }: { record: ExclusionRecord }): JSX.Element => (
  <section className="exclusion-record">
    <h3>
      <span className="exclusion-badge">excluded</span>
      <span className="mono">{record.reason}</span>
    </h3>
    {record.line !== "" && <p className="exclusion-line">{record.line}</p>}
    {record.callIds.length > 0 && (
      <p className="muted">
        calls: {record.callIds.map((callId) => <code key={callId}>{callId}</code>)}
      </p>
    )}
    {record.legs.length > 0 && <p className="muted">legs: {record.legs.join(", ")}</p>}
    {record.sut.length > 0 && (
      <p className="muted">
        SUT: {record.sut.map((address) => <code key={address}>{address}</code>)}
      </p>
    )}
    {record.groups.length > 0 && <p className="muted">call groups: {record.groups.join(", ")}</p>}
    <Evidence record={record} />
  </section>
)

/** The refusals of one case, or of every case the file names. `caseId` narrows a capture's whole file. */
export const ExclusionPane = (
  { exclusion, caseId }: { exclusion: CaseExclusion; caseId?: string | undefined }
): JSX.Element => {
  const read = parseExclusions(exclusion.document)
  if (!read.ok) {
    return <p className="error">{exclusion.path}: {read.reason}</p>
  }
  const records = caseId === undefined ? read.records : read.records.filter((record) => record.caseId === caseId)
  return (
    <div className="exclusion-pane">
      <p className="muted">
        No case was generated here. The pipeline refused it and kept its reason beside the cases
        it did write; the capture itself stays readable.
      </p>
      <p className="muted path">{exclusion.path}</p>
      {records.map((record) => <Record key={`${record.caseId}-${record.reason}`} record={record} />)}
      {read.warnings.length > 0 && (
        <ul className="warnings">
          {read.warnings.map((warning) => <li key={warning}>{warning}</li>)}
        </ul>
      )}
    </div>
  )
}
