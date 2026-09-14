/**
 * An auto ACK expectation's count is DRAWN by what makes the platform under
 * replay put an ACK on that leg (§6.3), never counted off the wire: the two
 * platforms are different UACs, and a captured platform that under-ACKed states
 * its own non-compliance rather than what the run will see.
 *
 * The count is the FINAL's own ladder, and nothing on the far leg puts an ACK
 * here: a B2BUA owes one ACK per final it RECEIVES on this leg (RFC 3261
 * §13.2.2.4 for a 2xx, §17.1.1.3 for a non-2xx), so N ACKs arriving from the
 * far leg draw none of their own.
 *
 * **Composed on arrival.** An ACK carrying no body is one the stack can form
 * the moment the final lands — in-dialog or not — so every copy of that final
 * draws one back. Only an ACK owing the ANSWER to a delayed offer (RFC 3261
 * §13.2.1) waits on the far leg: a copy arriving inside that wait is answered
 * by the single ACK that follows it, and only a copy landing once the ACK
 * exists draws one of its own.
 *
 * **Displaced by the next INVITE.** The stack holds ONE emitted ACK per leg,
 * reset on every new INVITE transaction (§6.1), so a copy landing once the SUT
 * has opened a later transaction on that leg answers a transaction the dialog
 * no longer holds an ACK for and draws none.
 *
 * The SEND half is the scripted actor's own behaviour and stays as captured: a
 * caller that holds its ACK, or answers three copies with one, is modelling a
 * peer, and a peer's non-compliance is the document's to state.
 */
import { Flows, Tokens } from "@sip/contracts"
import { type ClassName, rungIntervalsMs } from "@sip/contracts/schedules"
import type { StepDraft } from "./draft.js"
import type { StepSource } from "./flowsteps.js"

/** One expectation the pass gave a count, for the flag. */
export interface Drawn {
  readonly step: string
  readonly leg: string
  readonly final: string
  readonly captured: number
  /** Copies of the final that drew one. */
  readonly drawn: number
}

/**
 * Stamp every auto `expect` ACK with the count it draws, and report the ones
 * whose number the capture did not already hold. Mutates `steps`.
 */
export const stampDrawnAckCounts = (
  flows: Flows.FlowsDoc,
  steps: Array<StepDraft>,
  sources: ReadonlyArray<StepSource>
): Array<Drawn> => {
  const out: Array<Drawn> = []
  for (let i = 0; i < steps.length; i++) {
    const step = steps[i]!
    if (step.op !== "expect" || step.auto !== true) continue
    const msg = msgOf(flows, sources[i])
    if (msg === undefined || !Flows.isMethod(msg, "ACK")) continue
    const final = answeredInviteFinal(flows, steps, sources, i)
    if (final === undefined) continue
    const ladder = steps[final]!.retransmits ?? 0
    const drawn = composesOnArrival(step)
      ? ladder - displacedCopies(flows, steps, sources, final)
      : ladder - heldCopies(steps[final]!, step)
    const captured = step.retransmits ?? 0
    if (drawn === captured) continue
    if (drawn === 0) delete steps[i]!.retransmits
    else steps[i]!.retransmits = drawn
    out.push({
      step: step.id,
      leg: step.leg,
      final: steps[final]!.id,
      captured,
      drawn
    })
  }
  return out
}

/**
 * Whether the stack can form this ACK the moment the final arrives: the one
 * carrying no body, which it composes from the dialog alone. Only an ACK owing
 * the answer to a delayed offer (RFC 3261 §13.2.1) is composed from the far
 * leg's and does not exist until that one lands.
 *
 * Whether the ACK CONFIRMS the dialog says nothing about this: a re-INVITE's
 * ACK is as composable as an initial one when neither owes a body, and our
 * stack mints the ACK's client transaction on taking either 2xx.
 */
const composesOnArrival = (ack: StepDraft): boolean => {
  const body = ack.msg.body
  return body === undefined || ("mode" in body && body.mode === "absent")
}

/**
 * The wait before each of `final`'s `rungs` repeats, rung 1 first: the gaps
 * the document states where the capture measured them (§6.9), otherwise the
 * RFC's schedule for the final's class — the one table `sip-retransmit` walks
 * (ADR-0032 X1), read as data rather than walked again here. A count longer
 * than either list repeats the last gap, as `Schedule::exact` does past its
 * list: steady pacing rather than a class the step never chose.
 */
const rungGapsMs = (final: StepDraft, rungs: number): ReadonlyArray<number> => {
  const stated = final.retransmit_intervals_ms
  const gaps = stated !== undefined && stated.length > 0 ? stated : rungIntervalsMs(classOf(final))
  return Array.from({ length: rungs }, (_, r) => gaps[Math.min(r, gaps.length - 1)]!)
}

/**
 * The retransmission class the RFC puts on the final's emitter, read the way
 * the interpreter's audit reads it off the datagram: a 2xx to an INVITE rides
 * §13.3.1.4 (`final-2xx`), any other INVITE final §17.2.1 Timer G
 * (`invite-server-final`). The step IS an INVITE final — {@link answeredInviteFinal}
 * chose it — so those two are the only classes it can be, and a spec that
 * states no status is read as the non-2xx one: the two pace identically, and
 * a final with no status is a document lint has already refused.
 */
const classOf = (final: StepDraft): ClassName =>
  final.msg.status !== undefined && final.msg.status < 300 ? "final-2xx" : "invite-server-final"

/**
 * How many of `final`'s repeats landed before `ack` went out, and 0 wherever
 * the document states no coordinate to compare.
 *
 * The rungs are {@link rungGapsMs}: the final's own measured pacing where it
 * has one, the class's schedule where it has none. A rung at the ACK's own
 * instant is not held: the ACK exists, so the copy is one the platform
 * re-passes.
 */
const heldCopies = (final: StepDraft, ack: StepDraft): number => {
  const rungs = final.retransmits ?? 0
  if (rungs === 0) return 0
  if (final.observed === undefined || ack.observed === undefined) return 0
  const dwellMs = (ack.observed.at_us - final.observed.at_us) / 1_000
  let at = 0
  let held = 0
  for (const gap of rungGapsMs(final, rungs)) {
    at += gap
    if (at < dwellMs) held += 1
  }
  return held
}

/**
 * How many of `final`'s repeats land once a LATER INVITE transaction has opened
 * on the leg, which draw no ACK back.
 *
 * The stack's emitted ACK is held per leg and reset on every new INVITE
 * transaction (§6.1), so a copy arriving past that reset answers a transaction
 * the dialog holds no ACK for. Paced off the document's own declared timeline,
 * which is what the run follows: the capture's instants say nothing here, an
 * ACK composed on receipt having moved the re-INVITE ahead of a repeat the
 * source platform placed behind it.
 */
const displacedCopies = (
  flows: Flows.FlowsDoc,
  steps: ReadonlyArray<StepDraft>,
  sources: ReadonlyArray<StepSource>,
  final: number
): number => {
  const rungs = steps[final]!.retransmits ?? 0
  if (rungs === 0) return 0
  const reset = supersedingInvite(flows, steps, sources, final)
  if (reset === undefined) return 0
  const at = projectMs(steps)
  const resetMs = at[reset]! - at[final]!
  let rung = 0
  let displaced = 0
  for (const gap of rungGapsMs(steps[final]!, rungs)) {
    rung += gap
    if (rung >= resetMs) displaced += 1
  }
  return displaced
}

/**
 * The step index of the first INVITE the SUT puts on `final`'s leg on a LATER
 * transaction, or `undefined` where it opens none.
 *
 * Strictly later by CSeq: a repeat of the SAME INVITE rides the transaction it
 * already opened and resets nothing.
 */
const supersedingInvite = (
  flows: Flows.FlowsDoc,
  steps: ReadonlyArray<StepDraft>,
  sources: ReadonlyArray<StepSource>,
  final: number
): number | undefined => {
  const finalMsg = msgOf(flows, sources[final])
  if (finalMsg === undefined) return undefined
  for (let j = final + 1; j < steps.length; j++) {
    if (steps[j]!.leg !== steps[final]!.leg) continue
    if (sources[j]!.emits) continue
    const m = msgOf(flows, sources[j])
    if (m === undefined || !Flows.isMethod(m, "INVITE")) continue
    if (m.summary.cseq.seq <= finalMsg.summary.cseq.seq) continue
    return j
  }
  return undefined
}

/**
 * Each step's instant on the document's declared timeline (ms from the run's
 * trigger), walking each `delay` back to the anchor it names.
 */
const projectMs = (steps: ReadonlyArray<StepDraft>): ReadonlyArray<number> => {
  const index = new Map(steps.map((s, i) => [s.id, i]))
  const at = new Array<number>(steps.length).fill(0)
  const done = new Array<boolean>(steps.length).fill(false)
  const resolve = (i: number, seen: ReadonlySet<number>): number => {
    if (done[i]!) return at[i]!
    const anchor = Tokens.anchorStep(steps[i]!.delay.from)
    const j = anchor === undefined ? undefined : index.get(anchor)
    // A cycle states no timeline; the step's own dwell is all it can carry.
    at[i] = (j === undefined || seen.has(j) ? 0 : resolve(j, new Set(seen).add(i))) +
      steps[i]!.delay.ms
    done[i] = true
    return at[i]!
  }
  for (let i = 0; i < steps.length; i++) resolve(i, new Set([i]))
  return at
}

/**
 * The step index of the INVITE final an ACK answers: same pivot leg, EMITTED by
 * the scripted peer, same transaction and same dialog. The latest before the
 * ACK, so a re-INVITE's own final is never mistaken for the initial one's.
 *
 * Every final, not only a 2xx: which of the two ACKs a caller is looking at is
 * exactly what the final's STATUS says, and both oblige one ACK per copy.
 */
export const answeredInviteFinal = (
  flows: Flows.FlowsDoc,
  steps: ReadonlyArray<StepDraft>,
  sources: ReadonlyArray<StepSource>,
  ack: number
): number | undefined => {
  const ackMsg = msgOf(flows, sources[ack])
  if (ackMsg === undefined) return undefined
  for (let j = ack - 1; j >= 0; j--) {
    if (steps[j]!.leg !== steps[ack]!.leg) continue
    if (!sources[j]!.emits) continue
    const m = msgOf(flows, sources[j])
    if (m === undefined || m.summary.kind !== "response") continue
    if (m.summary.status < 200) continue
    if (m.summary.cseq.method.toUpperCase() !== "INVITE") continue
    if (m.summary.cseq.seq !== ackMsg.summary.cseq.seq) continue
    if (m.summary.to.tag !== ackMsg.summary.to.tag) continue
    return j
  }
  return undefined
}

const msgOf = (flows: Flows.FlowsDoc, src: StepSource | undefined): Flows.Msg | undefined =>
  src === undefined ? undefined : flows.legs[src.origLeg]?.msgs[src.msgIdx]
