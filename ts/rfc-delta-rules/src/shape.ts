/**
 * The `shape` section: a message the replay adds, leaves out or answers on its
 * own where the RFC leaves it to the stack or to arrival order, or where the
 * captured side broke the clause that states it, read off the ladder's shape
 * rather than a header; and the report a REFER's NOTIFY carries where the RFC
 * lets the stack report progress first.
 */
import { strays, substitution } from "@sip/delta-rules/families"
import { statusOf } from "@sip/delta-rules/scope"
import { autoAnswered, message, statusSubstitution, substituted, TRYING_DEADLINE_MS } from "@sip/delta-rules/shapes"
import type { RfcFacts } from "./facts.js"
import type { RfcRow } from "./row.js"

export const [prackAutomaticStray]: ReadonlyArray<RfcRow> = strays<RfcFacts>({
  PRACK: {
    id: "prack-automatic-stray",
    rfc: "RFC 3262 §4",
    why:
      "RFC 3262 §4 obliges the stack to emit and answer PRACK; it is a stack automatic of every " +
      "replay (ADR-0024) and never part of the captured choreography."
  }
})

export const byeTeardownCrossStray: RfcRow = {
  id: "bye-teardown-cross-stray",
  on: { kind: "shape", family: ["serviced-stray"] },
  rfc: "RFC 3261 §15.1.2",
  why:
    "The relayed teardown BYE reaches an endpoint whose script never expected it and is " +
    "auto-answered 200 (RFC 3261 §15.1.2): both ends released at once and the captured side " +
    "absorbed the cross, or the capture missed the datagram. Every leg ends and the CDR is written.",
  shape: (s) => autoAnswered(s, "BYE")
}

export const tryingOnNonInviteNotReproduced: RfcRow = {
  id: "trying-on-non-invite-not-reproduced",
  on: { kind: "shape", family: ["status-substitution"], scope: ["BYE", "CANCEL"] },
  gate: "tryingOnNonInviteSteps",
  rfc: "RFC 3261 §9.2, §15.1.2, §17.2.2",
  why:
    "A final in place of the capture's 100 to a BYE or CANCEL: RFC 3261 §17.2.2 leaves the 100 " +
    "to the stack and owes only the final, which must be the one the document's dialog state " +
    "owes (§9.2, §15.1.2, §12.2.2).",
  shape: (s, facts) => {
    const sub = statusSubstitution(s)
    const nonInvite = statusOf(s.scope, "BYE") !== undefined || statusOf(s.scope, "CANCEL") !== undefined
    return sub?.captured === 100 && nonInvite && facts.tryingOnNonInviteSteps.get(s.step) === sub.observed
  }
}

export const byeNotRelayedOntoCancelledEarlyDialog: RfcRow = {
  id: "bye-not-relayed-onto-cancelled-early-dialog",
  on: { kind: "shape", family: ["missing-message"] },
  gate: "byeBesideCancelSteps",
  rfc: "RFC 3261 §9.1, §15",
  why:
    "The BYE missing on a callee's early dialog the caller both CANCELled and BYEd: the CANCEL " +
    "alone ends the INVITE (RFC 3261 §9.1), so carrying the BYE onto the early dialog is left " +
    "to the stack. Gated on the document; a BYE missing on an answered dialog stays visible.",
  shape: (s, facts) => message(s, "missing-message", "BYE") && facts.byeBesideCancelSteps.has(s.step)
}

export const cancelledCallNotDialled: RfcRow = {
  id: "cancelled-call-not-dialled",
  on: { kind: "shape", family: ["missing-message"] },
  gate: "dialledAfterCancelSteps",
  rfc: "RFC 3261 §9.2",
  why:
    "The opening INVITE missing on a leg the capture dialled after the caller had CANCELled: RFC " +
    "3261 §9.2 has the UAS answer the CANCELled INVITE 487, where the captured side placed the " +
    "call first, and the replay dials nobody for it.",
  shape: (s, facts) => message(s, "missing-message", "INVITE") && facts.dialledAfterCancelSteps.has(s.step)
}

export const cancelPastAnswerNotRelayed: RfcRow = {
  id: "cancel-past-answer-not-relayed",
  on: { kind: "shape", family: ["missing-message", "method-substitution"] },
  gate: "cancelPastAnswerSteps",
  rfc: "RFC 3261 §9.1",
  why:
    "The CANCEL the capture relayed to a callee that had already answered 2xx, missing in the " +
    "replay: a CANCEL has no effect on a request that has its final (RFC 3261 §9.1), so " +
    "relaying it is left to the stack, and the callee's next message is the ACK or BYE.",
  shape: (s, facts) => {
    const kind = s.shapeKind
    const substitutedForCancel = kind.shape === "method-substitution" && kind.expected === "CANCEL" &&
      ["ACK", "BYE"].includes(kind.observed)
    return facts.cancelPastAnswerSteps.has(s.step) && (message(s, "missing-message", "CANCEL") || substitutedForCancel)
  }
}

export const answerRelayedAheadOfCrossingCancel: RfcRow = substitution<RfcFacts>({
  id: "answer-relayed-ahead-of-crossing-cancel",
  gate: "cancelCrossingAnswerSteps",
  rfc: "RFC 3261 §9.1, §9.2",
  why:
    "A 2xx where the capture answered the caller 487: the callee's 2xx left before her CANCEL " +
    "and is the INVITE's final (RFC 3261 §9.2), so which reaches her first is arrival order at " +
    "the relay. Exactly 487 to 2xx; the reverse order stays visible."
}, { captured: 487, observed: "2xx", at: "initial-invite" })

export const referProgressNotifyReport: RfcRow = {
  id: "refer-progress-notify-report",
  on: { kind: "body" },
  gate: "referOutcomeAtStep",
  rfc: "RFC 3515 §2.4.4, §2.4.5",
  why:
    "The sipfrag of the one NOTIFY the document scripts for a REFER: RFC 3515 §2.4.5 leaves the " +
    "stack free to report `100` first and the capture's outcome in a later terminal NOTIFY, " +
    "which the run shows on that leg. Another outcome stays visible.",
  body: (b, facts) => {
    const outcome = facts.referOutcomeAtStep.get(b.step)
    if (b.mediaType !== "message/sipfrag" || outcome === undefined) return false
    const status = (side: ReadonlyArray<string>): number | undefined => {
      const m = /^SIP\/2\.0\s+(\d{3})/.exec((side[0] ?? "").trim())
      return m === null ? undefined : Number(m[1])
    }
    return status(b.captured) === outcome && status(b.replayed) === 100
  }
}

export const tryingProvisionalUnscripted: RfcRow = {
  id: "trying-provisional-unscripted",
  on: { kind: "shape", family: ["status-substitution"], scope: ["INVITE"] },
  gate: "untriedInviteSteps",
  rfc: "RFC 3261 §17.2.1",
  why:
    "The replay's own 100 Trying where the document scripts none for an INVITE the party sent: " +
    "RFC 3261 §17.2.1 has the server transaction send one unless its TU answers within 200 ms, so " +
    "the captured side's omission is no evidence. Bounded to the transaction's own gap.",
  shape: (s, facts) => {
    const captured = statusSubstitution(s)?.captured
    return captured !== undefined && captured > 100 && facts.untriedInviteSteps.has(s.step) &&
      substituted(s, { observed: 100, cseq: "INVITE", at: "response" })
  }
}

export const tryingOmittedBeforeFinal: RfcRow = {
  id: "trying-omitted-before-final",
  on: { kind: "shape", family: ["trying-omitted"], scope: ["INVITE"] },
  rfc: "RFC 3261 §17.2.1",
  why:
    "The scripted 100 Trying the replay left out because its final went within 200 ms of the " +
    "INVITE: RFC 3261 §17.2.1 owes a 100 only past that. A later final with no 100 before it " +
    "stays visible.",
  shape: (s) =>
    s.shapeKind.shape === "trying-omitted" && s.shapeKind.afterMs <= TRYING_DEADLINE_MS &&
    statusOf(s.scope, "INVITE") !== undefined
}

export const requestPastReleaseAnswered481: RfcRow = substitution<RfcFacts>({
  id: "request-past-release-answered-481",
  gate: "roundsPastReleaseSteps",
  rfc: "RFC 3261 §12.2.2, §15.1.1, §15.1.2",
  why:
    "A round the peer opens after its leg's BYE, answered 481: a dialog ends when a BYE is " +
    "sent or received (RFC 3261 §15.1), and §12.2.2 has the stack answer a request matching " +
    "none 481, whatever the capture answered. Exactly 481, past the release."
}, { observed: 481, at: "response" })
