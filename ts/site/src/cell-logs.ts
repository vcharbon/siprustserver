/**
 * The logs of the call a cell placed, read for that call over its own
 * window: the cell's SUT log attachments (`sut-log`, `lane-log`), the SUT's
 * log already cut to the window, each labelled by the collector that read it
 * and whose log it is on the cell's lane. A cell that states no window, or
 * holds no SUT log, has none.
 */
import { callLog, callLogText, identitiesOf, refIdsOf } from "./call-log.ts"
import { sutLogRole } from "./lane-roles.ts"
import { cellWindow } from "./sut-evidence.ts"
import type { TriageCatalog } from "./triage.ts"

/** One log of the call. */
export interface CallLogRead {
  /** The collector that read it: the attachment's name. */
  readonly source: string
  /** How it was read. */
  readonly how: string
  /** Whose log it is on this lane. */
  readonly role: string | undefined
  /** It is the log of this lane's SUT. */
  readonly sut: boolean
  readonly lines: ReadonlyArray<string>
}

/** The logs of the call `detail` placed. */
export const callLogsOf = (detail: TriageCatalog.StudyCellDetail): ReadonlyArray<CallLogRead> => {
  if (cellWindow(detail) === undefined || detail.join === null) return []
  const identities = identitiesOf(detail.join.callIds, refIdsOf(detail.http?.exchanges ?? []))
  return detail.sutLogs.map((one): CallLogRead => ({
    source: one.name,
    how: "the cell's SUT log, cut to the call's window",
    role: sutLogRole(one.title),
    sut: true,
    lines: callLogText(callLog(one.text, identities), identities)
  }))
}
