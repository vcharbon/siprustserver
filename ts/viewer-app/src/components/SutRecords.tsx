/**
 * What joins one cell to the SUT's own records: the Call-IDs the cell minted,
 * which the SUT keys its call state and its log lines by, the wall-clock
 * window the cell drove it in, the run's hook logs cut to that window, and the
 * call state the SUT still held after the batch for this cell's calls.
 *
 * A window no log line falls inside says so rather than drawing nothing, so a
 * log that was not kept and a SUT that logged nothing read apart.
 */
import type { JSX } from "react"
import type { TriageCellJoin, TriageStudyHook } from "../api.ts"
import { JsonView } from "./JsonView.tsx"

export const SutRecords = ({ join, windows, held }: {
  readonly join: TriageCellJoin
  readonly windows: readonly TriageStudyHook[]
  readonly held: readonly unknown[]
}): JSX.Element => (
  <details className="study-record cell-sut" open>
    <summary>
      the SUT's records of this cell — {join.from} → {join.to}
    </summary>
    <p className="muted">
      Call-ID(s) minted:{" "}
      {join.callIds.map((callId) => <code key={callId} className="call-id">{callId}</code>)}
    </p>
    {held.length > 0 && (
      <div className="error">
        <p>the SUT still held call state of this cell after its batch:</p>
        <JsonView value={held} label="held" />
      </div>
    )}
    {windows.length === 0
      ? <p className="muted">no hook log is kept with this run</p>
      : windows.map((window) => (
        <details key={window.name} className="rerun-log study-hook" open>
          <summary className="muted">hook: {window.name}, this cell's window</summary>
          <pre className="raw">{window.text === "" ? "(no line of the log falls inside this window)" : window.text}</pre>
        </details>
      ))}
  </details>
)
