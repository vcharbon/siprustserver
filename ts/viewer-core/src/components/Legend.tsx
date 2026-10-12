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
  { key: "solid arrow", what: "scripted: the document composes and owns this message" },
  { key: "dashed arrow", what: "auto: the interpreter's own SIP stack owns it — scripted by nobody, recorded against the step" },
  { key: "dotted arrow", what: "optional: absence is tolerated, a later step on the leg releases it" },
  { key: "record", what: "the stored content is recorded for post-run confrontation and matched against nothing" },
  { key: "no record", what: "assert: the stored content is matched against the inbound message" },
  { key: "+Nms", what: "dwell from its anchor; amber means timer-linked, which is never compressible" },
  { key: "xN", what: "retransmissions of this message, collapsed onto the step it repeats" },
  { key: "after a, b", what: "cross-leg ordering: the step waits until those nodes completed" },
  { key: "<=Nms", what: "per-step expect budget, overriding the document's default" },
  { key: "races s7", what: "a declared overlap with that step" },
  { key: "N checks", what: "inline field assertions over the matched message" },
  { key: "deviation", what: "peer non-compliance the replay reproduces on purpose" },
  { key: "defect", what: "the step whose outcome IS the defect the case captures" },
  { key: "Nms", what: "capture or run offset from the first message" },
  {
    key: "alt sections",
    what:
      "declared alternatives, one stacked section each, separated by OR; the stack is a reading order, not a sequence — exactly one branch runs and they all rejoin below"
  },
  { key: "unordered", what: "bracketed steps all arrive, in any order" },
  { key: "inject bar", what: "an external event handed to the lane's injector; the interpreter never runs it" },
  {
    key: "fromA1 + toA1",
    what:
      "the dialog's simplified tag PAIR, never the real tags: both beads on one arrow, each at the end of the party holding that tag. Requests and responses of one dialog carry the same pair"
  },
  {
    key: "same from, different to",
    what:
      "a FORK: one leg answered under several early dialogs — toB1 and toB2 under one fromB1, one colour each. The numbering runs past the fork: the ACK, the re-INVITE and the BYE restate the tag of the early dialog that answered, so they keep its token and its colour"
  },
  {
    key: "dashed to— bead",
    what: "no To-tag yet, so no dialog is established: the initial request and its 100 Trying"
  },
  { key: "arrow colour", what: "which call the message belongs to, where a document holds several" }
]

/** The call colour strip: which colour is which `calls[]` entry. */
export const CallLegend = ({ layout }: { layout: LadderLayout }): JSX.Element | null => {
  // One call still earns the strip once a leg JOINED it: the joined leg sits on
  // its own branch, and unlabelled that reads as a parallel fork.
  const joined = layout.calls.some((call) => call.attempts.some((a) => a.joinedBy !== undefined))
  if (layout.calls.length < 2 && !joined) return null
  return (
    <div className="fv-call-legend">
      {layout.calls.map((call) => (
        <span key={call.id} className={`fv-call-chip ${callSlotClass(call.slot)}`}>
          <b>{call.id}</b>
          <span>
            {`caller ${call.callerLeg} · legs ${call.legs.join(", ")}`}
            {call.attempts.map((attempt) =>
              ` · ${attempt.branch}.${attempt.position} ${attempt.leg} -> ${attempt.callee}${
                attempt.finalStatus === undefined ? "" : ` (${attempt.finalStatus})`
              }${attempt.cause === undefined ? "" : ` ${attempt.cause}`}${
                attempt.joinedBy === undefined
                  ? ""
                  : ` ⤷ ${attempt.joinedBy.kind}@${attempt.joinedBy.step}`
              }`
            ).join("")}
          </span>
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
        <h4>calls in this document</h4>
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
