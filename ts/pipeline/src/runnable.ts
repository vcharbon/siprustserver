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
 *   The ACTOR sent an offer-less INVITE and took a 2xx, and the leg goes on to
 *   carry a new in-dialog transaction, which only a CONFIRMED dialog carries.
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
 * The two ACK rules are predictions about the RUN, so both read the OFFER MODEL
 * the exchange was dialled with ({@link offeredIngress}): an offer-carrying
 * INVITE draws its ACK out of the UAC that sent it, an offer-less one leaves
 * that ACK to travel end to end (RFC 3264 §4).
 */
import { Body, Flow } from "@sip/contracts"

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
 * decides who owes the ACK to its 2xx.
 *
 * A UAC that offered ACKs the 2xx on receipt, because the answer came back in
 * the response (RFC 3261 §13.2.2.4). A UAC that did NOT offer takes the offer
 * IN the 2xx and owes the answer in its ACK (RFC 3264 §4), and a B2BUA holding
 * that leg has no answer of its own — it waits for the ACK arriving on the leg
 * the offer-less INVITE came from, and relays it.
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
 * What makes the hole cost anything is that the RUN puts an ACK there, and
 * which ACK that is turns on the offer model ({@link offeredIngress}). Where the
 * exchange was dialled WITH an offer the SUT ACKs the 2xx on receipt and the
 * charge is unconditional. Where it was dialled without one the SUT's ACK
 * carries an answer only the ingress leg's own ACK supplies, so it goes out
 * exactly when that ACK arrives — and where the document holds none after this
 * 2xx, nothing lands, the case replays as written, and the coverage is kept.
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

/** One dialog-creating 2xx the ACTOR owed an ACK for, and what proves it sent one. */
export interface UnackedTakenFinal {
  /** The step id of the 2xx the leg took. */
  readonly final: string
  /** The id of the INVITE step the actor had sent on this leg. */
  readonly invite: string
  readonly leg: string
  /** The id of the later in-dialog request the leg carries. */
  readonly continuation: string
  /** The method that request names. */
  readonly method: string
  /** Where the 2xx sits in the capture, where the step states it. */
  readonly observed?: Flow.Observed
  /** Where the INVITE sits in the capture, where the step states it. */
  readonly inviteObserved?: Flow.Observed
  /** Where the continuing request sits in the capture, where the step states it. */
  readonly continuationObserved?: Flow.Observed
}

/** The finding before the continuation that proves it. */
type UnackedTaken = Omit<UnackedTakenFinal, "continuation" | "method">

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
 * Every dialog-creating 2xx the actor takes, never ACKs, and goes on to use.
 * Empty for a document cut from a vantage that captured its whole call.
 *
 * The mirror of {@link unackedFinals}: there the peer answers the SUT and the
 * SUT owes the ACK, here the actor dials and the ACTOR owes it (RFC 3261
 * §13.2.2.4). The cost is worse than a stray datagram — the peer leg's ACK step
 * is gated on a message that never comes, taking the run down at that step.
 *
 * DELAYED-OFFER only, because that is the only dial the peer leg's ACK waits
 * for. An actor that offered draws an offer-carrying INVITE out of the SUT, and
 * the SUT ACKs the peer's 2xx on receipt whatever this leg does; an actor that
 * did not leaves the SUT owing an answer it can only take from this leg's ACK
 * (RFC 3264 §4), so withholding it strands the peer's ACK step.
 *
 * The continuation is the second discriminator, because an actor that truly
 * never ACKs is a corner case worth REPLAYING — our own reaper answers it — and
 * looks identical up to this point. So the charge rests on what the leg carries
 * AFTERWARDS: a new in-dialog transaction ({@link isContinuation}) is traffic no
 * unconfirmed dialog carries, and its presence means the ACK crossed the wire
 * and the trace lost it. Where the leg carries none, the abandoned-dialog
 * reading stands and the case is kept.
 *
 * One forward pass per leg. Charged on the 2xx, so an INVITE the actor sent and
 * nobody answered is untouched, and a non-2xx final is out of scope: its ACK is
 * the client transaction's (§17.1.1.3), composed from the final itself.
 */
export const unackedTakenFinals = (
  flow: ReadonlyArray<Flow.FlowNode>
): ReadonlyArray<UnackedTakenFinal> => {
  const steps = flow.flatMap((node) => Flow.flowNodeSteps(node))
  const opened = new Map<string, Flow.Step>()
  const owed = new Map<string, { readonly at: number; readonly final: UnackedTaken }>()
  const charged: Array<UnackedTakenFinal> = []
  const settle = (leg: string): void => {
    const pending = owed.get(leg)
    owed.delete(leg)
    if (pending === undefined) return
    const next = steps.slice(pending.at + 1).find((s) => s.leg === leg && isContinuation(s))
    if (next === undefined) return
    charged.push({
      ...pending.final,
      continuation: next.id,
      method: method(next),
      ...(next.observed === undefined ? {} : { continuationObserved: next.observed })
    })
  }
  steps.forEach((step, at) => {
    if (isInvite(step)) {
      settle(step.leg)
      if (step.op === "send" && !carriesBody(step)) opened.set(step.leg, step)
      else opened.delete(step.leg)
    } else if (step.op === "expect" && isInviteSuccess(step)) {
      const invite = opened.get(step.leg)
      if (invite === undefined) return
      owed.set(step.leg, {
        at,
        final: {
          final: step.id,
          invite: invite.id,
          leg: step.leg,
          ...(step.observed === undefined ? {} : { observed: step.observed }),
          ...(invite.observed === undefined ? {} : { inviteObserved: invite.observed })
        }
      })
    } else if (step.op === "send" && isAck(step)) owed.delete(step.leg)
  })
  for (const leg of [...owed.keys()]) settle(leg)
  return charged
}

/** One charged 2xx as the refusal's line states it. */
const unackedTakenClause = (c: UnackedTakenFinal): string => {
  const where = (o: Flow.Observed | undefined): string =>
    o === undefined ? "" : ` (capture leg ${o.leg} msg ${o.msg})`
  return `leg ${c.leg} step ${c.final}${where(c.observed)} answers the actor's INVITE at ` +
    `${c.invite}${where(c.inviteObserved)} and the leg captured no ACK for it, yet goes on ` +
    `to carry the ${c.method} at ${c.continuation}${where(c.continuationObserved)}`
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
