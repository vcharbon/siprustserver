/**
 * What joins one cell to the SUT's own records: the Call-IDs the cell minted,
 * which the SUT keys its call state and its log lines by, the wall-clock
 * window the cell drove it in, the cell's SUT logs over that window (the rig's
 * `sut-log` collector's attachments), and the call state the SUT still held
 * after the batch for this cell's calls.
 *
 * A log no line falls inside says so rather than drawing nothing, so a cell
 * whose log was not collected and a SUT that logged nothing read apart.
 */
import type { JSX } from "react"
import type { TriageCellJoin, TriageSutLog } from "../api.ts"
import { JsonView } from "./JsonView.tsx"

export const SutRecords = ({ join, sutLogs, held }: {
  readonly join: TriageCellJoin
  readonly sutLogs: readonly TriageSutLog[]
  readonly held: readonly unknown[]
}): JSX.Element => (
  <details className="study-record cell-sut" open>
    <summary>
      SUT records <span className="count">{join.from} → {join.to}</span>
    </summary>
    <p className="muted">
      Call-IDs{" "}
      {join.callIds.map((callId) => <code key={callId} className="call-id">{callId}</code>)}
    </p>
    {held.length > 0 && (
      <div className="error">
        <p>call state held after the batch</p>
        <JsonView value={held} label="held" />
      </div>
    )}
    {sutLogs.length === 0
      ? <p className="muted">no SUT log collected</p>
      : sutLogs.map((log, index) => (
        <details key={`${log.title}-${index}`} className="rerun-log study-hook" open>
          <summary className="muted">
            {log.title} · this cell <span className="count">{log.bytes} B</span>
          </summary>
          <pre className="raw">{log.text === "" ? "no line in this window" : log.text}</pre>
        </details>
      ))}
  </details>
)
