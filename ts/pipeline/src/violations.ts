/**
 * STAMPING the allowed-errors registry onto a case: the RFC violations a source
 * capture is known to carry, resolved against the vantage this case was cut
 * from.
 *
 * The registry is the committed memory (`@sip/contracts` `AllowedErrors`); this
 * module is the half that decides what it MEANS for one document. A listed call
 * the case was not cut from contributes nothing. A listed call whose violating
 * message no step carries at this vantage WARNS — dropping that silently would
 * turn a flagged capture into a clean case the moment a vantage changed.
 */
import { AllowedErrors, Violation, type Case, type Flow, type Flows, type Placement } from "@sip/contracts"
import type { StepSource } from "./flowsteps.js"
import { body, headerValue } from "./wire.js"

export interface StampInput {
  readonly registry: AllowedErrors.AllowedErrors | undefined
  /** The source capture's file name — the registry's key. */
  readonly capture: string
  readonly flows: Flows.FlowsDoc
  /** The Call-IDs of the case's own correlated calls (`cut.ts::caseCallIds`). */
  readonly callIds: ReadonlyArray<string>
  readonly sources: ReadonlyArray<StepSource>
  readonly steps: ReadonlyArray<Flow.Step>
  readonly legs: ReadonlyArray<Placement.Leg>
}

export interface Stamped {
  readonly violations: Array<Violation.RfcViolation>
  /** One line per entry that matched the case but could not be anchored. */
  readonly warnings: Array<string>
}

/** The registry's entries for this capture, resolved against this case. */
export const stampRfcViolations = (input: StampInput): Stamped => {
  const out: Stamped = { violations: [], warnings: [] }
  const entries = AllowedErrors.violationsOf(input.registry, input.capture)
  if (entries.length === 0) return out

  // The CUT's calls, not the flows document's group: a group joins calls that
  // merely passed one element, and an entry naming a call the cut left outside
  // the SUT is not this case's to stamp.
  const caseCallIds = new Set(input.callIds)
  const stepById = new Map(input.steps.map((s) => [s.id, s]))
  const actorOfLeg = new Map(input.legs.map((l) => [l.id, l.actor]))

  for (const e of entries) {
    for (const callId of e.call_ids) {
      if (!caseCallIds.has(callId)) continue
      const anchor = anchorOf(input, e, callId, stepById, actorOfLeg)
      if (anchor === undefined) {
        out.warnings.push(
          `${input.capture}: allowed-errors entry '${e.rule}' on call-id '${callId}' ` +
            `(originator ${e.originator}) is NOT stamped — no step carries ${MESSAGE_OF[e.rule]} ` +
            `at this case's vantage`
        )
        continue
      }
      if (!out.violations.some((v) => v.rule === anchor.rule && v.step === anchor.step)) {
        out.violations.push(anchor)
      }
    }
  }
  return out
}

/** A warning as the generator's own diagnostic sidecar carries it. */
export const unanchoredFlag = (detail: string): Case.Flag => ({
  kind: "rfc-violation-unanchored",
  detail
})

/** What a rule's anchor message IS, in the words a warning uses. */
const MESSAGE_OF: Record<Violation.RfcRule, string> = {
  "no-200-after-cancel": "that 200 to INVITE",
  "unacked-reliable-provisional": "that reliable provisional",
  "no-ack-to-dialog-creating-2xx": "that dialog-creating 2xx",
  "no-cancel-after-final": "that late CANCEL",
  "second-answer-repeats-the-first": "that second answer"
}

/**
 * The step carrying the violating message, at this case's vantage, and who the
 * rule charges for it.
 *
 * The rules anchor on opposite sides of the same arrow, because one rule's
 * offence is a message SENT and the others' is a message never sent in reply:
 *
 * | rule | anchor | charged |
 * |---|---|---|
 * | `no-200-after-cancel` | the first 2xx to an INVITE the originator EMITTED | its sender |
 * | `unacked-reliable-provisional` | the first reliable provisional the originator TOOK | its receiver |
 * | `no-ack-to-dialog-creating-2xx` | the first dialog-creating 2xx the originator TOOK | its receiver |
 * | `no-cancel-after-final` | the CANCEL the originator EMITTED past its own final | its sender |
 * | `second-answer-repeats-the-first` | the SECOND binding answer the originator EMITTED on one dialog | its sender |
 *
 * The last one is the only rule whose anchor is not the first message its
 * predicate admits — the first binding answer is the compliant one — so its
 * anchors are read off the leg ahead of the walk.
 *
 * `emitter` therefore follows the step's own direction: the vantage's actor is
 * the charged party, or the party sits on the platform side and the document
 * names it `sut` — which is what gates.
 */
const anchorOf = (
  input: StampInput,
  entry: AllowedErrors.AllowedViolation,
  callId: string,
  stepById: ReadonlyMap<string, Flow.Step>,
  actorOfLeg: ReadonlyMap<string, string>
): Violation.RfcViolation | undefined => {
  const charges =
    entry.rule === "unacked-reliable-provisional" ||
      entry.rule === "no-ack-to-dialog-creating-2xx"
      ? "receiver"
      : "sender"
  const seconds = entry.rule === "second-answer-repeats-the-first"
    ? secondAnswers(input.flows, callId, entry.originator)
    : undefined
  for (const src of input.sources) {
    const leg = input.flows.legs[src.origLeg]
    if (leg === undefined || leg.call_id !== callId) continue
    const msg = leg.msgs[src.msgIdx]
    if (msg === undefined) continue
    const party = charges === "sender" ? msg.src : msg.dst
    if (party !== entry.originator) continue
    if (seconds !== undefined) {
      if (!seconds.has(`${src.origLeg}:${src.msgIdx}`)) continue
    } else if (!isAnchorFor(entry.rule, msg)) continue
    const step = stepById.get(src.id)
    if (step === undefined) continue
    const actorIsCharged = charges === "sender" ? step.op === "send" : step.op === "expect"
    const emitter = actorIsCharged ? actorOfLeg.get(step.leg) : Violation.SUT_EMITTER
    if (emitter === undefined) continue
    return { rule: entry.rule, step: step.id, emitter }
  }
  return undefined
}

/**
 * Every message on `callId` that `originator` sent stating a BINDING answer on
 * a dialog it had already stated one on, as `<leg>:<index>` keys — the second
 * and every later one, never the first, which is the answer the peer acts on.
 *
 * A binding answer is a non-failure response to INVITE carrying a session
 * description, in a final or a reliable provisional (RFC 3261 §13.2.1,
 * RFC 3262 §5) — the reading `rfc_rules`'s own rule takes. A retransmission
 * states no new answer (§17) and is passed over.
 */
const secondAnswers = (
  flows: Flows.FlowsDoc,
  callId: string,
  originator: string
): ReadonlySet<string> => {
  const out = new Set<string>()
  flows.legs.forEach((leg, legIdx) => {
    if (leg.call_id !== callId) return
    const stated = new Set<string>()
    leg.msgs.forEach((msg, msgIdx) => {
      if (msg.repeat_of !== undefined) return
      if (msg.src !== originator) return
      if (!bindsAnAnswer(msg)) return
      const dialog = msg.summary.kind === "response" ? (msg.summary.to.tag ?? "") : ""
      if (stated.has(dialog)) out.add(`${legIdx}:${msgIdx}`)
      else stated.add(dialog)
    })
  })
  return out
}

/** Whether `msg` states an answer the peer is bound to act on. */
const bindsAnAnswer = (msg: Flows.Msg): boolean => {
  if (msg.summary.kind !== "response") return false
  if (msg.summary.cseq.method.toUpperCase() !== "INVITE") return false
  const status = msg.summary.status
  if (status < 101 || status >= 300) return false
  if (body(msg)?.mediaType.toLowerCase() !== "application/sdp") return false
  return status >= 200 || isReliableProvisional(msg, status)
}

/** Whether `msg` is the message the rule's evidence rests on. */
const isAnchorFor = (rule: Violation.RfcRule, msg: Flows.Msg): boolean => {
  // RFC 3261 §9.1: the offence is the CANCEL itself, the one request in this
  // vocabulary a rule anchors on.
  if (rule === "no-cancel-after-final") {
    return (
      msg.summary.kind === "request" && (msg.summary.method ?? "").toUpperCase() === "CANCEL"
    )
  }
  if (msg.summary.kind !== "response") return false
  if (msg.summary.cseq.method.toUpperCase() !== "INVITE") return false
  const status = msg.summary.status
  if (rule === "no-200-after-cancel") return status >= 200 && status < 300
  // RFC 3261 §13.2.2.4: only a 2xx carrying a To tag confirms a dialog, and
  // only a confirmed dialog draws the ACK this rule misses.
  if (rule === "no-ack-to-dialog-creating-2xx") {
    return status >= 200 && status < 300 && msg.summary.to.tag !== null
  }
  // The second answer is not the first message its own predicate admits, so it
  // anchors off the LEG ahead of the walk (see `secondAnswers`), never here.
  if (rule === "second-answer-repeats-the-first") return false
  return isReliableProvisional(msg, status)
}

/** RFC 3262 §3: reliable means BOTH headers, and 100 is never reliable. */
const isReliableProvisional = (msg: Flows.Msg, status: number): boolean =>
  status > 100 &&
  status < 200 &&
  hasOptionTag(msg, "Require", "100rel") &&
  headerValue(msg, "RSeq") !== undefined

/** Whether `msg`'s `name` header lists `tag` (RFC 3261 §7.3.1: a comma set). */
const hasOptionTag = (msg: Flows.Msg, name: string, tag: string): boolean =>
  (headerValue(msg, name) ?? "")
    .split(",")
    .some((t) => t.trim().toLowerCase() === tag.toLowerCase())
