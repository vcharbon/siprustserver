/**
 * The refusals decided on the DOCUMENT rather than on the capture: a vantage
 * that captured only part of an exchange.
 *
 * A transaction is one exchange, and a vantage holding some of it did not
 * capture the call. The document transcribes what the vantage held, so it
 * inherits the hole, and the replay is then judged against an exchange no
 * conformant SUT will reproduce. Four rules, one concern:
 *
 * - {@link unfinalledAcks} — the ACK is there and the final is not. An ACK is
 *   owed only to a final that ARRIVED (RFC 3261 §13.2.2.4, §17.1.1.3), so the
 *   ACK is the PROOF the answer crossed that socket and the trace lost it.
 * - {@link unackedFinals} — the final is there and the ACK is not. The SUT sent
 *   the INVITE and took a 2xx, so the SUT owes the ACK; the capture holds none.
 * - {@link unackedTakenFinals} — the same hole on the other side of the arrow.
 *   The ACTOR sent the INVITE and took a 2xx, and either the leg goes on to
 *   carry a new in-dialog transaction, which only a CONFIRMED dialog carries, or
 *   the 2xx's ladder stopped short of its schedule and the far leg expects the
 *   ACK relayed.
 * - {@link orphanResponses} — the response is there and the request is not, with
 *   the method left open. A response belongs to a transaction, so a leg holding
 *   one and not the request that opened it lost that request to the trace,
 *   whatever the method was.
 * They cost the replay the same way. Where no leg captured the missing half, the
 * step composes out of nothing — a transaction-derived ACK draws its R-URI,
 * Route set, Via and CSeq from a final that is not there (§6.3), and a response
 * step answers a transaction the actor never receives — and the run refuses at
 * the step. Where the PEER leg captured it, the B2BUA relays it and it arrives on
 * a leg holding no step for it, an unexpected datagram on a document that is
 * otherwise complete.
 *
 * Only a transaction-derived (`auto`) ACK is judged by the first two. A scripted
 * ACK carries its own stored content and states its own coordinates, so it is
 * emittable whatever the flow holds around it.
 *
 * The two ACK rules are predictions about the RUN, and the prediction is that an
 * ACK to a 2xx travels END TO END: the stack relays the acknowledging party's
 * own (RFC 3261 §13.2.2.4), so a leg whose ACK the trace lost holds a step
 * nothing satisfies. Where that ACK never comes, the §13.3.1.4 give-up composes
 * only the one owing no answer body ({@link offeredIngress}, RFC 3264 §4), and
 * {@link unackedFinals} charges on that.
 */
import { Body, Flow, Schedules, Tokens } from "@sip/contracts"
import { PROXIMITY_US } from "./delay.js"
import { inviteTransactions } from "./transactions.js"

/** The reason token a document ACKing an unfinalled transaction is refused by. */
export const FINAL_NOT_CAPTURED = "source-final-not-captured"

/** One ACK whose final the leg never captured, and the transaction it names. */
export interface UnfinalledAck {
  /** The ACK step's id. */
  readonly ack: string
  /** The id of the INVITE step its leg had outstanding. */
  readonly invite: string
  readonly leg: string
  /** Where the ACK sits in the capture, where the step states it. */
  readonly observed?: Flow.Observed
  /** Where the INVITE sits in the capture, where the step states it. */
  readonly inviteObserved?: Flow.Observed
}

const method = Flow.stepMethod

const isInvite = (step: Flow.Step): boolean => Flow.isRequest(step, "INVITE")

const isAck = (step: Flow.Step): boolean => Flow.isRequest(step, "ACK")

const isAutoAck = (step: Flow.Step): boolean => step.auto === true && isAck(step)

const isRequest = (step: Flow.Step): boolean =>
  step.msg.status === undefined && step.msg.method !== undefined

/**
 * Whether the step's message carries a body. A `shape: absent` declaration is
 * not one: the document is stating that the message had none.
 */
const carriesBody = (step: Flow.Step): boolean => {
  const body = step.msg.body
  if (body === undefined) return false
  return !(Body.isShapeBody(body) && body.mode === "absent")
}

/**
 * Whether the document states this message as REPEATED — a ladder the capture
 * measured, which the replay re-paces (§6.9).
 */
const repeats = (step: Flow.Step): boolean => (step.retransmits ?? 0) > 0

/**
 * Whether the INVITE that opened this exchange carried an OFFER, which is what
 * decides whether this stack can compose the ACK to its 2xx ALONE.
 *
 * A UAC that offered has its answer in the response (RFC 3261 §13.2.2.4), so
 * that ACK owes no body and the stack can form it from dialog state. A UAC that
 * did NOT offer takes the offer IN the 2xx and owes the answer in its ACK
 * (RFC 3264 §4), which only the far party supplies. Either one is the far
 * party's own, relayed; what the offer decides is what the stack can put on the
 * leg when that ACK never comes — the §13.3.1.4 give-up acknowledges the first
 * before its BYE and leaves the second to the BYE alone.
 *
 * Read at the INGRESS, never on the SUT's own INVITE, because what the SUT
 * emits is what the document CAPTURED and a platform is free to re-offer where
 * ours relays: the offer model of the run is the one the ACTOR dialled with.
 * Where no ingress INVITE precedes it, the SUT originated the exchange and
 * offered as any UAC does.
 */
const offeredIngress = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): boolean => {
  for (let i = at - 1; i >= 0; i -= 1) {
    const step = steps[i]!
    if (step.leg !== leg && step.op === "send" && isInvite(step)) return carriesBody(step)
  }
  return true
}

const isInviteFinal = Flow.isFinalToInvite

/**
 * Every ACK the flow owes a final its own leg never states. Empty for a document
 * cut from a vantage that captured its whole call.
 *
 * One forward pass per leg: an INVITE opens the leg's transaction — RFC 3261
 * §14.1 leaves one outstanding per dialog, so the newest is the only one — and
 * an ACK is charged when no final has answered it since.
 */
export const unfinalledAcks = (
  flow: ReadonlyArray<Flow.FlowNode>
): ReadonlyArray<UnfinalledAck> => {
  const opened = new Map<string, Flow.Step>()
  const finalled = new Set<string>()
  const charged: Array<UnfinalledAck> = []
  for (const step of flow.flatMap((node) => Flow.flowNodeSteps(node))) {
    if (isInvite(step)) {
      opened.set(step.leg, step)
      finalled.delete(step.leg)
    } else if (isInviteFinal(step)) finalled.add(step.leg)
    else if (isAutoAck(step) && !finalled.has(step.leg)) {
      const invite = opened.get(step.leg)
      if (invite === undefined) continue
      charged.push({
        ack: step.id,
        invite: invite.id,
        leg: step.leg,
        ...(step.observed === undefined ? {} : { observed: step.observed }),
        ...(invite.observed === undefined ? {} : { inviteObserved: invite.observed })
      })
    }
  }
  return charged
}

/** One charged ACK as the refusal's line states it. */
const clause = (c: UnfinalledAck): string => {
  const where = (o: Flow.Observed | undefined): string =>
    o === undefined ? "" : ` (capture leg ${o.leg} msg ${o.msg})`
  return `leg ${c.leg} step ${c.ack}${where(c.observed)} ACKs the INVITE at ${c.invite}` +
    `${where(c.inviteObserved)} and the leg captured no final answering it`
}

/** The one-line finding, for stderr and for `excluded.json`. */
export const unfinalledLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<UnfinalledAck>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${FINAL_NOT_CAPTURED} — ` +
  charged.map(clause).join("; ")

/**
 * The reason token a document holding a 2xx the SUT owed the ACK for is refused
 * by. Its mirror is {@link ACTOR_ACK_NOT_CAPTURED}: the two are
 * opposite-direction rules and each answers for itself.
 */
export const ACK_NOT_CAPTURED = "source-ack-not-captured"

/** One 2xx the SUT owed an ACK for, and the transaction it answered. */
export interface UnackedFinal {
  /** The step id of the 2xx the leg sent. */
  readonly final: string
  /** The id of the INVITE step the SUT had sent to this leg. */
  readonly invite: string
  readonly leg: string
  /** Where the 2xx sits in the capture, where the step states it. */
  readonly observed?: Flow.Observed
  /** Where the INVITE sits in the capture, where the step states it. */
  readonly inviteObserved?: Flow.Observed
}

const isInviteSuccess = Flow.isSuccessToInvite

const isAckArrival = (step: Flow.Step): boolean => step.op === "expect" && isAck(step)

/**
 * Every 2xx the flow answers a SUT INVITE with and never states the ACK for.
 * Empty for a document cut from a vantage that captured its whole call.
 *
 * A BYE anywhere around it changes nothing, and that is the whole point of the
 * rule: RFC 5407 §2 puts a UA that has sent OR received a BYE in the Mortal
 * state, where it "MUST NOT send any new requests within the dialog" — and
 * carves the ACK to a 2xx straight back out of that prohibition, because the
 * ACK belongs to the INVITE transaction and not to the dialog (RFC 3261 §6,
 * §17.1). Appendix D is explicit that invite-usage state is kept past the BYE
 * for this one purpose. So the ACK is owed unconditionally, the capture that
 * lacks it lost it, and a conformant SUT replaying the document sends it and is
 * charged an unexpected datagram.
 *
 * What makes the hole cost anything is that the RUN still puts an ACK there, and
 * whether it can turns on the offer model ({@link offeredIngress}). Where the
 * exchange was dialled WITH an offer the stack can compose that ACK alone, so
 * the §13.3.1.4 give-up puts one on the leg ahead of its BYE and the charge is
 * unconditional. Where it was dialled without one the ACK owes an answer only
 * the ingress leg's own ACK supplies, so it goes out exactly when that ACK
 * arrives — and where the document holds none after this 2xx, nothing lands,
 * the case replays as written, and the coverage is kept.
 *
 * TWO SHAPES ARE NOT THIS HOLE, and each is read off the document itself:
 *
 * - an ACK the leg DOES capture settles its transaction for good, so every later
 *   repeat of that 2xx is one the answer crossed in flight (§13.3.1.4) — the ACK
 *   is on the wire and the trace holds it;
 * - a 2xx the document states as REPEATED drew no ACK on the WIRE, because a UAS
 *   retransmits a 2xx only while none has arrived (§13.3.1.4). That is the
 *   source breaking §13.2.2.4, not a datagram the vantage missed, and a stated
 *   source violation is replayed rather than refused.
 *
 * One forward pass per leg. Charged on the 2xx, not on the INVITE, so a leg the
 * SUT never answered is untouched. A non-2xx final is out of scope: its ACK is
 * the client transaction's (§17.1.1.3), hop by hop, and rides the final's own
 * retransmissions rather than the dialog.
 */
export const unackedFinals = (
  flow: ReadonlyArray<Flow.FlowNode>
): ReadonlyArray<UnackedFinal> => {
  const steps = flow.flatMap((node) => Flow.flowNodeSteps(node))
  const opened = new Map<string, { readonly step: Flow.Step; readonly at: number }>()
  const owed = new Map<
    string,
    { readonly charge: boolean; readonly final: UnackedFinal }
  >()
  /** Legs whose open INVITE transaction the flow states an ACK for. */
  const acked = new Set<string>()
  const charged: Array<UnackedFinal> = []
  /**
   * Whether an ACK the SUT can carry onto this leg is sent anywhere else once
   * the exchange is open. Measured from the INVITE and not from the 2xx: the
   * answer the ACK carries reaches the SUT once, and every 2xx of that
   * transaction after it — the original and each repeat — draws its own ACK.
   */
  const forwarded = (at: number, leg: string): boolean =>
    steps.slice(at + 1).some((s) => s.leg !== leg && s.op === "send" && isAck(s))
  const supersede = (leg: string): void => {
    const pending = owed.get(leg)
    owed.delete(leg)
    if (pending !== undefined && pending.charge) charged.push(pending.final)
  }
  steps.forEach((step, at) => {
    if (isInvite(step)) {
      supersede(step.leg)
      acked.delete(step.leg)
      if (step.op === "expect") opened.set(step.leg, { step, at })
      else opened.delete(step.leg)
    } else if (step.op === "send" && isInviteSuccess(step)) {
      const invite = opened.get(step.leg)
      if (invite === undefined || acked.has(step.leg) || repeats(step)) return
      owed.set(step.leg, {
        charge: offeredIngress(steps, invite.at, step.leg) ||
          forwarded(invite.at, step.leg),
        final: {
          final: step.id,
          invite: invite.step.id,
          leg: step.leg,
          ...(step.observed === undefined ? {} : { observed: step.observed }),
          ...(invite.step.observed === undefined ? {} : { inviteObserved: invite.step.observed })
        }
      })
    } else if (isAckArrival(step)) {
      owed.delete(step.leg)
      acked.add(step.leg)
    }
  })
  for (const leg of [...owed.keys()]) supersede(leg)
  return charged
}

/** One charged 2xx as the refusal's line states it. */
const unackedClause = (c: UnackedFinal): string => {
  const where = (o: Flow.Observed | undefined): string =>
    o === undefined ? "" : ` (capture leg ${o.leg} msg ${o.msg})`
  return `leg ${c.leg} step ${c.final}${where(c.observed)} answers the SUT's INVITE at ` +
    `${c.invite}${where(c.inviteObserved)} and the leg captured no ACK for it`
}

/** The one-line finding, for stderr and for `excluded.json`. */
export const unackedLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<UnackedFinal>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${ACK_NOT_CAPTURED} — ` +
  charged.map(unackedClause).join("; ")

/**
 * The reason token a document holding a 2xx the ACTOR owed the ACK for is
 * refused by. The mirror of {@link ACK_NOT_CAPTURED} across the arrow, and its
 * own token because a replay falsifying one says nothing about the other.
 */
export const ACTOR_ACK_NOT_CAPTURED = "source-actor-ack-not-captured"

/**
 * What proves the actor's missing ACK crossed the wire: a later in-dialog
 * request on the leg (`continuation`), or a 2xx whose ladder stopped short of
 * its schedule while the far leg expects its ACK relayed (`relayed-ack`).
 */
export type LostAckGroundKind = "continuation" | "relayed-ack"

/** Where a 2xx's declared ladder stopped, in ms from the 2xx. */
export interface LadderEnd {
  /** How many rungs the document declares. */
  readonly rungs: number
  /** The instant of the last declared rung. */
  readonly lastRungMs: number
  /** The instant the next rung of the schedule was due. */
  readonly dueMs: number
}

/** One dialog-creating 2xx the ACTOR owed an ACK for, and the step that proves it sent one. */
interface UnackedTaken {
  /** The step id of the 2xx the leg took. */
  readonly final: string
  /** The id of the INVITE step the actor had sent on this leg. */
  readonly invite: string
  readonly leg: string
  /** Where the 2xx sits in the capture, where the step states it. */
  readonly observed?: Flow.Observed
  /** Where the INVITE sits in the capture, where the step states it. */
  readonly inviteObserved?: Flow.Observed
}

/** The finding with its ground: one arm per proof. */
export type UnackedTakenFinal =
  & UnackedTaken
  & {
    /** The id of the step that proves it. */
    readonly proof: string
    /** Where the proving step sits in the capture, where the step states it. */
    readonly proofObserved?: Flow.Observed
  }
  & (
    | {
      readonly ground: "continuation"
      /** The method the later in-dialog request on the leg names. */
      readonly method: string
    }
    | {
      readonly ground: "relayed-ack"
      /** The far leg whose ACK step is the actor's ACK relayed. */
      readonly groundLeg: string
      /** How long the leg stayed silent after the 2xx, in ms: past the rung that was due. */
      readonly silenceMs: number
      /** The ladder the 2xx ran before it stopped; absent where it declares none. */
      readonly ladder?: LadderEnd
    }
  )

/**
 * Whether a request on the leg is one only a CONFIRMED dialog carries. ACK and
 * CANCEL belong to the INVITE transaction rather than the dialog (RFC 3261 §6),
 * and a BYE is what a platform reaping an un-ACKed 2xx sends — so all three
 * leave an abandoned dialog looking exactly like a confirmed one, and none of
 * them counts. Every other in-dialog request is a NEW transaction the peer only
 * opens, or only answers, once the ACK has confirmed the dialog (§12.2, §14.1).
 */
const isContinuation = (step: Flow.Step): boolean =>
  isRequest(step) && !["ACK", "BYE", "CANCEL"].includes(method(step))

/**
 * Every 2xx the actor takes on a leg and no ACK on that leg ever settles, each
 * at its position in the step list, in document order — the SETTLE predicate,
 * apart from what proves the missing ACK was lost.
 *
 * Leg state names the transaction an ACK settles, never the step's captured
 * `cseq` ({@link inviteTransactions}): the newest INVITE the actor sent that
 * holds a final and no ACK yet, each ACK its own. A non-2xx final consumes an
 * ACK the same way (§17.1.1.3) and is never owed. A second final on the same
 * INVITE — a fork's 2xx, a re-emission — is its own step owed its own ACK
 * (§13.2.2.4); a repeat the document folds is no step. A 2xx the actor takes
 * on a leg it never sent an INVITE on is nobody's to settle here.
 */
const unsettledTakenFinals = (
  steps: ReadonlyArray<Flow.Step>
): ReadonlyArray<{ readonly at: number; readonly final: UnackedTaken }> =>
  [...inviteTransactions(steps).byLeg.values()]
    .flat()
    .flatMap((t) => {
      if (
        t.op !== "send" ||
        t.invite === undefined ||
        t.acked ||
        t.final === undefined ||
        !isInviteSuccess(t.final.step)
      ) {
        return []
      }
      const final = t.final.step
      const invite = t.invite
      return [{
        at: t.final.at,
        final: {
          final: final.id,
          invite: invite.id,
          leg: final.leg,
          ...(final.observed === undefined ? {} : { observed: final.observed }),
          ...(invite.observed === undefined ? {} : { inviteObserved: invite.observed })
        }
      }]
    })
    .sort((a, b) => a.at - b.at)

/**
 * Whether the request at `at` is an INVITE the leg's next INVITE final travelling
 * the other way answers 491: RFC 6026 Accepted glare, the answering side stating
 * an earlier INVITE is still un-ACKed at that moment — the opposite of a
 * confirmed dialog.
 */
const answeredRequestPending = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): boolean => {
  const request = steps[at]!
  if (!isInvite(request)) return false
  const final = steps
    .slice(at + 1)
    .find((s) => s.leg === leg && s.op !== request.op && isInviteFinal(s))
  return final?.msg.status === 491
}

/**
 * The first new in-dialog transaction the leg carries after the 2xx
 * ({@link isContinuation}), traffic no unconfirmed dialog carries. A re-INVITE
 * the peer answered 491 is not that traffic ({@link answeredRequestPending});
 * the continuation after it still is. Undefined where the leg carries none.
 */
const continuationAfter = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): Flow.Step | undefined => {
  for (let i = at + 1; i < steps.length; i += 1) {
    const s = steps[i]!
    if (s.leg !== leg || !isContinuation(s) || answeredRequestPending(steps, i, leg)) continue
    return s
  }
  return undefined
}

/** The wait before each rung of a 2xx's ladder, T1 first, to its give-up (RFC 3261 §13.3.1.4). */
const FINAL_2XX_RUNGS_MS = Schedules.rungIntervalsMs("final-2xx")

/**
 * Where the declared ladder of the 2xx stopped: its rung count, the instant of
 * its last rung — the gaps the document states summed, the schedule's where it
 * states none ({@link Schedules.rungGapsMs}) — and the instant the schedule's
 * next rung was due. A 2xx declaring no rung is a ladder that stopped at its
 * head, the first rung due at T1. Undefined where the ladder ran to the
 * schedule's end: no rung was due after it.
 */
const ladderEnd = (step: Flow.Step): LadderEnd | undefined => {
  const rungs = step.retransmits ?? 0
  const next = FINAL_2XX_RUNGS_MS[rungs]
  if (next === undefined) return undefined
  const lastRungMs = Schedules.rungGapsMs("final-2xx", rungs, step.retransmit_intervals_ms)
    .reduce((sum, gap) => sum + gap, 0)
  return { rungs, lastRungMs, dueMs: lastRungMs + next }
}

/**
 * Whether the far leg's ACK step sits AFTER the ladder's last rung, by the two
 * `observed` instants. Vacuous where the ladder declares no rung; false where
 * either coordinate is unstated, since nothing then sets the ACK against the
 * rung.
 */
const ackAfterLastRung = (twoxx: Flow.Step, ack: Flow.Step, ladder: LadderEnd): boolean => {
  if (ladder.rungs === 0) return true
  const from = twoxx.observed?.at_us
  const to = ack.observed?.at_us
  return from !== undefined && to !== undefined && to > from + ladder.lastRungMs * 1000
}

/** Whether the step ends the dialog it sits in, whichever side sends it. */
const endsDialog = (step: Flow.Step): boolean =>
  isRequest(step) && ["BYE", "CANCEL"].includes(method(step))

/**
 * How long the leg stayed silent after the 2xx at `at`, in ms, as the capture
 * measured it: from the 2xx's `observed.at_us` to the first same-leg step that
 * ends the dialog ({@link endsDialog}), or the leg's last step where none
 * does. The ladder the platform owed is folded over the whole envelope, so a
 * step that keeps the dialog going does not close the window. Undefined where
 * either coordinate is unstated or the leg carries nothing after the 2xx.
 */
const silenceAfter = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): number | undefined => {
  const rest = steps.slice(at + 1).filter((s) => s.leg === leg)
  const bound = rest.find(endsDialog) ?? rest.at(-1)
  const from = steps[at]!.observed?.at_us
  const to = bound?.observed?.at_us
  if (from === undefined || to === undefined) return undefined
  return Math.floor((to - from) / 1000)
}

const isRelayedSuccess = (step: Flow.Step, leg: string): boolean =>
  step.leg !== leg && step.op === "send" && isInviteSuccess(step)

/**
 * The index of the far leg's 2xx the SUT relayed onto `leg` as the 2xx at
 * `at`: the step the 2xx's delay is anchored on, where the cut stamped a
 * cross-leg 2xx-to-INVITE there — the relay the classifier read (§6.9) — and
 * otherwise the nearest cross-leg 2xx sent within relay proximity of the 2xx's
 * own instant, before or after it in the merged order: two captures, two
 * clocks, and the classifier anchors a relay stamped later than its arrival
 * on the arrival's own leg. -1 where none.
 */
const relayedSuccessOf = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): number => {
  const step = steps[at]!
  const anchor = Tokens.anchorStep(step.delay.from)
  const stamped = anchor === undefined ? -1 : steps.findIndex((s) => s.id === anchor)
  if (stamped >= 0 && isRelayedSuccess(steps[stamped]!, leg)) return stamped
  const t = step.observed?.at_us
  if (t === undefined) return -1
  let nearest = -1
  steps.forEach((s, i) => {
    const u = s.observed?.at_us
    if (!isRelayedSuccess(s, leg) || u === undefined || Math.abs(u - t) >= PROXIMITY_US) return
    if (nearest < 0 || Math.abs(u - t) < Math.abs(steps[nearest]!.observed!.at_us - t)) nearest = i
  })
  return nearest
}

/**
 * The far leg's ACK step to the transaction whose 2xx the SUT relayed onto this
 * leg at `at` ({@link relayedSuccessOf}): the first ACK that leg EXPECTS after
 * its 2xx, up to the far leg's next INVITE, which owns every ACK past it.
 * Undefined where the far leg expects none.
 */
const relayedAckAfter = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): Flow.Step | undefined => {
  const relayed = relayedSuccessOf(steps, at, leg)
  if (relayed < 0) return undefined
  const far = steps[relayed]!.leg
  for (let i = relayed + 1; i < steps.length; i += 1) {
    const s = steps[i]!
    if (s.leg !== far) continue
    if (isInvite(s)) return undefined
    if (isAckArrival(s)) return s
  }
  return undefined
}

/** What proves the missing ACK was lost, as {@link lostAckGround} finds it. */
type LostAckGround =
  | { readonly kind: "continuation"; readonly step: Flow.Step }
  | {
    readonly kind: "relayed-ack"
    readonly step: Flow.Step
    readonly silenceMs: number
    readonly ladder: LadderEnd
  }

/**
 * What proves the ACK to an unsettled 2xx CROSSED THE WIRE and the trace lost
 * it. Two proofs, the first that holds:
 *
 * - a continuation on the leg ({@link continuationAfter});
 * - the far leg expects that ACK relayed ({@link relayedAckAfter}) and the
 *   platform's 2xx ladder toward the actor STOPPED SHORT of its schedule: a
 *   UAS repeats a 2xx from T1 on, rung after rung to the give-up, until the
 *   ACK arrives (§13.3.1.4), so a ladder the document declares ended
 *   ({@link ladderEnd}) before the silence the capture measured
 *   ({@link silenceAfter}) would have carried the next scheduled rung is one
 *   the ACK reached — after its last rung, where the far leg's ACK step then
 *   sits ({@link ackAfterLastRung}). A silence that ends before the next rung
 *   was due gave the ladder no instant to fire, a ladder run to the schedule's
 *   end is the platform stating no ACK came, and a rung fired past the far
 *   leg's ACK reads nothing off its end, so all three keep the
 *   abandoned-dialog reading. The far leg's ACK is read as the actor's relayed,
 *   which a platform that ACKs the far leg on its own would falsify.
 *
 * Undefined where neither holds, and the abandoned-dialog reading stands.
 */
const lostAckGround = (
  steps: ReadonlyArray<Flow.Step>,
  at: number,
  leg: string
): LostAckGround | undefined => {
  const continuation = continuationAfter(steps, at, leg)
  if (continuation !== undefined) return { kind: "continuation", step: continuation }
  const twoxx = steps[at]!
  const ladder = ladderEnd(twoxx)
  if (ladder === undefined) return undefined
  const silenceMs = silenceAfter(steps, at, leg)
  if (silenceMs === undefined || silenceMs <= ladder.dueMs) return undefined
  const relayed = relayedAckAfter(steps, at, leg)
  if (relayed === undefined || !ackAfterLastRung(twoxx, relayed, ladder)) return undefined
  return { kind: "relayed-ack", step: relayed, silenceMs, ladder }
}

/**
 * Every dialog-creating 2xx the actor takes, never ACKs, and demonstrably ACKed.
 * Empty for a document cut from a vantage that captured its whole call.
 *
 * The mirror of {@link unackedFinals}: there the peer answers the SUT and the
 * SUT owes the ACK, here the actor dials and the ACTOR owes it (RFC 3261
 * §13.2.2.4). The cost is worse than a stray datagram — the peer leg's ACK step
 * is gated on a message that never comes, taking the run down at that step.
 *
 * EVERY dial, offer or none, because the ACK the SUT owes the peer leg IS this
 * one, relayed (§13.2.2.4): an actor that took the 2xx and never ACKed leaves
 * that peer step waiting on a datagram the document never scripts, whatever the
 * offer model was.
 *
 * Two questions, answered apart. Whether an ACK on the leg SETTLES the 2xx
 * ({@link unsettledTakenFinals}) reads the whole leg: an ACK the leg captures
 * for that transaction settles it wherever it sits, before or after a
 * continuation, and only a 2xx no ACK on the leg ever answers is charged. What
 * proves the missing ACK was LOST rather than never sent
 * ({@link lostAckGround}) is the second discriminator, because an actor that
 * truly never ACKs is a corner case worth REPLAYING — our own reaper answers
 * it — and looks identical up to this point. The far leg's ACK step is the
 * actor's ACK relayed: where the actor never sent one it waits on a datagram
 * the document never scripts, and the case is refused, never completed from
 * the far leg's step.
 *
 * Charged on the 2xx, so an INVITE the actor sent and nobody answered is
 * untouched, and a non-2xx final is out of scope: its ACK is the client
 * transaction's (§17.1.1.3), composed from the final itself.
 */
export const unackedTakenFinals = (
  flow: ReadonlyArray<Flow.FlowNode>
): ReadonlyArray<UnackedTakenFinal> => {
  const steps = flow.flatMap((node) => Flow.flowNodeSteps(node))
  return unsettledTakenFinals(steps).flatMap(({ at, final }) => {
    const ground = lostAckGround(steps, at, final.leg)
    if (ground === undefined) return []
    const proof = ground.step
    const stated = {
      ...final,
      proof: proof.id,
      ...(proof.observed === undefined ? {} : { proofObserved: proof.observed })
    }
    return [
      ground.kind === "continuation"
        ? { ...stated, ground: ground.kind, method: method(proof) }
        : {
          ...stated,
          ground: ground.kind,
          groundLeg: proof.leg,
          silenceMs: ground.silenceMs,
          ...(ground.ladder.rungs === 0 ? {} : { ladder: ground.ladder })
        }
    ]
  })
}

/** One charged 2xx as the refusal's line states it. */
const unackedTakenClause = (c: UnackedTakenFinal): string => {
  const where = (o: Flow.Observed | undefined): string =>
    o === undefined ? "" : ` (capture leg ${o.leg} msg ${o.msg})`
  const proof = c.ground === "continuation"
    ? `goes on to carry the ${c.method} at ${c.proof}${where(c.proofObserved)}`
    : c.ladder === undefined
    ? `the 2xx never repeated over the ${c.silenceMs} ms the leg stayed silent and leg ` +
      `${c.groundLeg} step ${c.proof}${where(c.proofObserved)} expects that ACK relayed`
    : `the 2xx's ladder stopped after ${c.ladder.rungs} rung${c.ladder.rungs === 1 ? "" : "s"} ` +
      `at +${c.ladder.lastRungMs} ms where the next was due at +${c.ladder.dueMs} ms, over the ` +
      `${c.silenceMs} ms the leg stayed silent, and leg ${c.groundLeg} step ${c.proof}` +
      `${where(c.proofObserved)} expects that ACK relayed after that rung`
  return `leg ${c.leg} step ${c.final}${where(c.observed)} answers the actor's INVITE at ` +
    `${c.invite}${where(c.inviteObserved)} and the leg captured no ACK for it, yet ${proof}`
}

/** The one-line finding, for stderr and for `excluded.json`. */
export const unackedTakenLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<UnackedTakenFinal>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${ACTOR_ACK_NOT_CAPTURED} — ` +
  charged.map(unackedTakenClause).join("; ")

/** The reason token a document answering an uncaptured request is refused by. */
export const REQUEST_NOT_CAPTURED = "source-request-not-captured"

/** One response step whose leg states no request it can answer. */
export interface OrphanResponse {
  /** The response step's id. */
  readonly response: string
  readonly leg: string
  readonly status: number
  /** The method of the transaction the response names. */
  readonly method: string
  /** Where the response sits in the capture, where the step states it. */
  readonly observed?: Flow.Observed
}

/**
 * Which side of a leg a request opens its transaction on. A request the actor
 * SENDS is answered by responses it EXPECTS, and the reverse — so a response is
 * matched against the requests travelling the other way.
 */
const openedBy = (leg: string, op: Flow.Step["op"]): string => `${leg}|${op}`

const answers = (step: Flow.Step): string =>
  openedBy(step.leg, step.op === "send" ? "expect" : "send")

/**
 * Every response the flow states on a leg that opened no transaction for it.
 * Empty for a document cut from a vantage that captured its whole call.
 *
 * One forward pass. A request opens its method on its leg and direction; a final
 * closes it, EXCEPT for an INVITE, which lives until its ACK — a 2xx
 * retransmits until the ACK arrives (RFC 3261 §13.3.1.4), so closing on the
 * first final would charge the retransmission. The ACK opens nothing: it draws
 * no response of its own (§17.1.1.3).
 */
export const orphanResponses = (
  flow: ReadonlyArray<Flow.FlowNode>
): ReadonlyArray<OrphanResponse> => {
  const open = new Map<string, Array<string>>()
  const charged: Array<OrphanResponse> = []
  const at = (key: string): Array<string> => {
    const held = open.get(key)
    if (held !== undefined) return held
    const fresh: Array<string> = []
    open.set(key, fresh)
    return fresh
  }
  const close = (key: string, method: string): void => {
    const held = at(key)
    const i = held.indexOf(method)
    if (i >= 0) held.splice(i, 1)
  }
  for (const step of flow.flatMap((node) => Flow.flowNodeSteps(node))) {
    if (isRequest(step)) {
      const m = method(step)
      // The ACK travels with its INVITE, so it closes the side that opened it.
      if (m === "ACK") close(openedBy(step.leg, step.op), "INVITE")
      else at(openedBy(step.leg, step.op)).push(m)
      continue
    }
    const status = step.msg.status
    if (status === undefined) continue
    const m = (step.msg["cseq-method"] ?? "").toUpperCase()
    const key = answers(step)
    if (!at(key).includes(m)) {
      charged.push({
        response: step.id,
        leg: step.leg,
        status,
        method: m,
        ...(step.observed === undefined ? {} : { observed: step.observed })
      })
      continue
    }
    if (status >= 200 && m !== "INVITE") close(key, m)
  }
  return charged
}

/** One charged response as the refusal's line states it. */
const orphanClause = (c: OrphanResponse): string => {
  const where = c.observed === undefined ? "" : ` (capture leg ${c.observed.leg} msg ${c.observed.msg})`
  return `leg ${c.leg} step ${c.response}${where} carries a ${c.status} to ${c.method} ` +
    `and the leg captured no ${c.method} it answers`
}

/** The one-line finding, for stderr and for `excluded.json`. */
export const orphanLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<OrphanResponse>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${REQUEST_NOT_CAPTURED} — ` +
  charged.map(orphanClause).join("; ")
