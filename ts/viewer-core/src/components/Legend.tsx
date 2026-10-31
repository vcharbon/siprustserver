/**
 * The legend: what the ladder's marks mean, one line each.
 *
 * The notation list is static — it describes the drawing, not a document. The
 * call and dialog strips are derived, because which calls and which forked
 * dialogs exist is a fact about the document in front of the reader.
 */
import type { JSX } from "react"
import { callSlotClass } from "../layout/calls.ts"
import type { LadderLayout } from "../layout/model.ts"

interface Notation {
  readonly key: string
  readonly what: string
}

/** One line per mark a ladder can draw. */
export const notations: readonly Notation[] = [
  { key: "solid arrow", what: "scripted: the document owns the message" },
  { key: "dashed arrow", what: "auto: the interpreter's SIP stack sends it" },
  { key: "dotted arrow", what: "optional: absence tolerated" },
  { key: "record", what: "content recorded for confrontation, not matched" },
  { key: "no record", what: "assert: content matched against the inbound message" },
  { key: "+Nms", what: "dwell from its anchor; amber = timer-linked, never compressed" },
  { key: "xN", what: "retransmissions, collapsed onto the step" },
  { key: "after a, b", what: "waits until those nodes complete" },
  { key: "<=Nms", what: "per-step expect budget" },
  { key: "races s7", what: "declared overlap with that step" },
  { key: "N checks", what: "inline field assertions" },
  { key: "deviation", what: "peer non-compliance reproduced on purpose" },
  { key: "defect", what: "the step whose outcome is the defect" },
  { key: "Nms", what: "offset from the first message" },
  { key: "alt sections", what: "declared alternatives separated by OR; one runs, all rejoin" },
  { key: "unordered", what: "bracketed steps arrive in any order" },
  { key: "inject bar", what: "external event for the lane's injector" },
  { key: "fromA1 + toA1", what: "the dialog's simplified tag pair, never the real tags" },
  { key: "same from, different to", what: "a fork: one leg, several early dialogs, one colour each" },
  { key: "dashed to— bead", what: "no To-tag yet: no dialog established" },
  { key: "arrow colour", what: "the call the message belongs to" }
]

/** One call in a line: its caller, its legs, each attempt and how it ended. */
const callLine = (call: LadderLayout["calls"][number]): string =>
  `caller ${call.callerLeg} · legs ${call.legs.join(", ")}` +
  call.attempts.map((attempt) =>
    ` · ${attempt.branch}.${attempt.position} ${attempt.leg} -> ${attempt.callee}${
      attempt.finalStatus === undefined ? "" : ` (${attempt.finalStatus})`
    }${attempt.cause === undefined ? "" : ` ${attempt.cause}`}${
      attempt.joinedBy === undefined
        ? ""
        : ` ⤷ ${attempt.joinedBy.kind}@${attempt.joinedBy.step}`
    }`
  ).join("")

/** The call colour strip: which colour is which `calls[]` entry. */
export const CallLegend = ({ layout }: { layout: LadderLayout }): JSX.Element | null => {
  // One call still earns the strip once a leg JOINED it: the joined leg sits on
  // its own branch, and unlabelled that reads as a parallel fork.
  const joined = layout.calls.some((call) => call.attempts.some((a) => a.joinedBy !== undefined))
  if (layout.calls.length < 2 && !joined) return null
  return (
    <div className="fv-call-legend">
      {layout.calls.map((call) => (
        <span key={call.id} className={`fv-call-chip ${callSlotClass(call.slot)}`} title={callLine(call)}>
          <b>{call.id}</b>
          <span>{callLine(call)}</span>
        </span>
      ))}
    </div>
  )
}

/**
 * The dialog strip, drawn only where a leg forked into several early dialogs.
 * Each chip states the whole pair, so the fork reads off the strip the way it
 * reads off the ladder: one from-token, several to-tokens.
 */
export const DialogLegend = ({ layout }: { layout: LadderLayout }): JSX.Element | null => {
  const forked = layout.dialogs.filter((dialog) => dialog.forked)
  if (forked.length === 0) return null
  const fromOf = (legId: string): string =>
    layout.dialogs.find((dialog) => dialog.side === "from" && dialog.legId === legId)?.token ?? ""
  return (
    <div className="fv-dialog-legend">
      {forked.map((dialog) => (
        <span
          key={dialog.token}
          className={`fv-dialog-chip fv-fork-${dialog.ordinal}`}
          title={dialog.earlyId === undefined
            ? `leg ${dialog.legId}, before it named any early dialog`
            : `leg ${dialog.legId}, early dialog "${dialog.earlyId}"`}
        >
          {`${fromOf(dialog.legId)} · ${dialog.token}${
            dialog.earlyId === undefined ? "" : ` = early ${dialog.earlyId}`
          }`}
        </span>
      ))}
    </div>
  )
}

export interface LadderLegendProps {
  readonly layout: LadderLayout
  readonly open?: boolean | undefined
}

/** The collapsible notation panel, with this document's calls and forks under it. */
export const LadderLegend = ({ layout, open }: LadderLegendProps): JSX.Element => (
  <details className="fv-legend" open={open ?? false}>
    <summary>legend</summary>
    <h4>notation</h4>
    <ul>
      {notations.map((notation) => (
        <li key={notation.key}>
          <span className="fv-legend-key">{notation.key}</span>
          <span className="fv-legend-what">{notation.what}</span>
        </li>
      ))}
    </ul>
    {layout.calls.length > 1 && (
      <>
        <h4>calls</h4>
        <CallLegend layout={layout} />
      </>
    )}
    {layout.dialogs.some((dialog) => dialog.forked) && (
      <>
        <h4>forked early dialogs</h4>
        <DialogLegend layout={layout} />
      </>
    )}
  </details>
)
