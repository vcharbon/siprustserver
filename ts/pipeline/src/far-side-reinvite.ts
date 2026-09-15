/**
 * The far side of an in-dialog INVITE exchange the capture holds on ONE leg
 * only, because the vantage lost the other leg past the answer it sent (§6.9).
 *
 * A leg whose captured record ENDS at a 2xx to INVITE its peer sends — no ACK,
 * not one datagram behind it — states nothing about the dialog past that
 * instant: not that it went quiet, only that the vantage stopped seeing it.
 * The near leg goes on, in either direction. Its peer sends an in-dialog
 * INVITE and takes a 2xx nothing on the far leg relays, so synthesis reads
 * that answer as minted by the platform; or the platform sends an in-dialog
 * INVITE down the near leg that nothing on the far leg relays into, and its
 * peer answers it. A platform that RELAYS such an INVITE end to end (the
 * policy's reading, `CasePolicy.relaysReinvite`) has the far leg on the other
 * end of both, where a document scripting nothing for it can neither answer
 * nor send: the near leg's expect waits for a message nobody composes, and
 * the run ends at its budget.
 *
 * So each such exchange is transcribed onto the far leg from the halves the
 * capture does hold, in the direction it ran:
 *
 * - the near peer's re-INVITE: an `expect INVITE` mirroring the near leg's
 *   send, its body compared by content where it carried one; a `send` of the
 *   2xx the near leg received, headers and body as captured, since a relayed
 *   answer is the far party's own; and an auto `expect ACK` mirroring the near
 *   leg's, where the capture holds one;
 * - the far party's re-INVITE: a `send INVITE` carrying the offer and the
 *   frozen headers the near leg's expect took, since a relayed request is the
 *   far party's own; an `expect` of the 2xx the near peer sent, its body
 *   compared as the session description the near leg emitted; and an auto
 *   `send ACK` carrying what the near leg's expect took, the answer where the
 *   offer was delayed.
 *
 * Each derived step keeps the `observed` coordinate of the near-leg message it
 * copies — the platform relays that message, so it is what the far-leg
 * datagram is compared against — and its source states the fact (`mirrored`:
 * the message is another leg's). Listed as the relay runs — a far-leg send
 * before the near-leg arrival it relays into, a far-leg arrival after the
 * near-leg send that relays into it — so the classifier reads the near leg's
 * half as the relay it is (§6.9). `far-side-reinvite-derived` names every one.
 *
 * Bounded three ways, each ruling out a shape the capture does state. The far
 * leg's record must end at its 2xx: a leg the vantage kept watching that shows
 * no INVITE says the platform did NOT relay, which a replay must surface. The
 * near-leg answer must be a 2xx, whoever sent it: a refusal the platform
 * composed is its own — except a 491, glare the replaying platform answers
 * itself (RFC 3261 §14.1), which owes the far leg nothing — and a refusal the
 * near peer sent is ACKed hop by hop (§17.1.1.3), by a far-leg transaction no
 * captured message stands for; `far-side-reinvite-not-derived` says which
 * exchange was left. And the near-leg half must have no relay origin: a
 * message the far leg's record does hold is already a step.
 */
import { Body, Tokens } from "@sip/contracts"
import { relayOriginOf, type StepTiming } from "./delay.js"
import type { MsgSpecDraft, StepDraft } from "./draft.js"
import { stepId, type StepSource } from "./flowsteps.js"

/** Who sent the re-INVITE: the near leg's peer, or the far party through the platform. */
export type ReinviteOrigin = "near-peer" | "far-party"

/** One exchange transcribed onto the far leg, as step OBJECTS until ids settle. */
export interface FarSide {
  readonly farLeg: string
  readonly origin: ReinviteOrigin
  /** The far leg's last captured step: the 2xx its record ends at. */
  readonly ended: StepDraft
  /** The near leg's own steps: the re-INVITE, its 2xx and its ACK. */
  readonly nearInvite: StepDraft
  readonly nearAnswer: StepDraft
  readonly nearAck?: StepDraft
  /** The derived steps, in document order: the ops the near leg's mirror. */
  readonly invite: StepDraft
  readonly answer: StepDraft
  readonly ack?: StepDraft
}

/** One exchange the pass declined, and the reason it states. */
export interface FarSideLeft {
  readonly farLeg: string
  readonly nearInvite: StepDraft
  readonly detail: string
}

/** The build-up arrays this pass splices into, all parallel to `steps`. */
export interface FarSideInput {
  readonly steps: Array<StepDraft>
  readonly timings: Array<StepTiming>
  readonly sources: Array<StepSource>
}

export interface FarSideOut {
  readonly derived: ReadonlyArray<FarSide>
  readonly left: ReadonlyArray<FarSideLeft>
}

const REWRITE: ReadonlyArray<string> = ["c=addr", "m=port"]

/** The 2xx to INVITE a leg's record may end at. */
const isSuccessToInvite = (step: StepDraft): boolean =>
  step.msg.status !== undefined &&
  step.msg.status >= 200 &&
  step.msg.status < 300 &&
  (step.msg["cseq-method"] ?? "").toUpperCase() === "INVITE"

const isFinalToInvite = (step: StepDraft): boolean =>
  step.msg.status !== undefined &&
  step.msg.status >= 200 &&
  (step.msg["cseq-method"] ?? "").toUpperCase() === "INVITE"

const isRequest = (step: StepDraft, method: string): boolean =>
  step.msg.status === undefined && (step.msg.method ?? "").toUpperCase() === method

/**
 * Derive the far side of every relayed in-dialog INVITE exchange whose far leg
 * the vantage lost. Mutates every array of `input` in step; ids are renumbered
 * once every insertion is in, and the records hold step objects until then.
 */
export const deriveFarSideReinvites = (input: FarSideInput): FarSideOut => {
  const { sources, steps, timings } = input
  const derived: Array<FarSide> = []
  const left: Array<FarSideLeft> = []

  /** Every leg whose record ends at a 2xx to INVITE its peer sends, by that step's index. */
  const blind: Array<{ leg: string; at: number }> = []
  const lastOf = new Map<string, number>()
  steps.forEach((s, i) => lastOf.set(s.leg, i))
  for (const [leg, at] of lastOf) {
    const step = steps[at]!
    if (step.op === "send" && isSuccessToInvite(step)) blind.push({ leg, at })
  }
  if (blind.length === 0) return { derived, left }

  interface Planned {
    readonly farLeg: string
    readonly origin: ReinviteOrigin
    readonly ended: number
    readonly invite: number
    readonly answer: number
    readonly ack: number | undefined
  }
  const planned: Array<Planned> = []
  for (const { leg: farLeg, at: ended } of blind) {
    // The near leg: the one whose expect of a 2xx relays the far leg's own.
    const relayed = steps.findIndex(
      (s, i) => s.op === "expect" && s.leg !== farLeg && relayOriginOf(timings, i) === ended
    )
    if (relayed < 0) continue
    const nearLeg = steps[relayed]!.leg
    for (let i = relayed + 1; i < steps.length; i++) {
      const s = steps[i]!
      if (s.leg !== nearLeg || !isRequest(s, "INVITE")) continue
      const origin: ReinviteOrigin = s.op === "send" ? "near-peer" : "far-party"
      // A relayed request the far leg's record holds is already a step.
      if (origin === "far-party" && relayOriginOf(timings, i) >= 0) continue
      const cseq = timings[i]!.cseq
      const answerOp = s.op === "send" ? "expect" : "send"
      const answer = steps.findIndex(
        (a, j) =>
          j > i &&
          a.leg === nearLeg &&
          a.op === answerOp &&
          isFinalToInvite(a) &&
          timings[j]!.cseq === cseq
      )
      if (answer < 0) continue
      if (origin === "near-peer" && relayOriginOf(timings, answer) >= 0) continue
      if (!isSuccessToInvite(steps[answer]!)) {
        // A 491 to the near peer is glare (RFC 3261 §14.1): the platform's own
        // answer to an INVITE crossing one it still holds open, which the
        // replaying platform composes itself. Nothing is owed on the far leg,
        // and nothing is left.
        if (origin === "near-peer" && steps[answer]!.msg.status === 491) continue
        left.push({
          farLeg,
          nearInvite: s,
          detail:
            origin === "near-peer"
              ? `the platform answered it ${steps[answer]!.msg.status} itself, which is its own ` +
                `refusal and no far party's`
              : `the near peer refused it ${steps[answer]!.msg.status}, and the far leg ACKs a ` +
                `refusal hop by hop (RFC 3261 §17.1.1.3), a transaction no captured message ` +
                `stands for`
        })
        continue
      }
      const ack = steps.findIndex(
        (a, j) =>
          j > answer &&
          a.leg === nearLeg &&
          a.op === s.op &&
          isRequest(a, "ACK") &&
          timings[j]!.cseq === cseq
      )
      if (origin === "far-party" && !emittable(steps[i]!.msg.body)) {
        left.push({
          farLeg,
          nearInvite: s,
          detail: `the offer it carried is held by shape only, and no far-leg send emits a shape`
        })
        continue
      }
      if (origin === "far-party" && ack >= 0 && !emittable(steps[ack]!.msg.body)) {
        left.push({
          farLeg,
          nearInvite: s,
          detail: `the answer its ACK carried is held by shape only, and no far-leg send emits a shape`
        })
        continue
      }
      planned.push({
        farLeg,
        origin,
        ended,
        invite: i,
        answer,
        ack: ack < 0 ? undefined : ack
      })
    }
  }
  if (planned.length === 0) return { derived, left }

  // Resolved to objects before any splice moves an index.
  const resolved = planned.map((p) => ({
    farLeg: p.farLeg,
    origin: p.origin,
    ended: steps[p.ended]!,
    nearInvite: steps[p.invite]!,
    nearAnswer: steps[p.answer]!,
    nearAck: p.ack === undefined ? undefined : steps[p.ack]!,
    at: p,
    invite:
      p.origin === "near-peer"
        ? deriveInvite(p.farLeg, steps[p.invite]!)
        : deriveSentInvite(p.farLeg, steps[p.invite]!),
    answer:
      p.origin === "near-peer"
        ? deriveAnswer(p.farLeg, steps[p.answer]!)
        : deriveExpectedAnswer(p.farLeg, steps[p.answer]!),
    ack:
      p.ack === undefined
        ? undefined
        : p.origin === "near-peer"
          ? deriveAck(p.farLeg, steps[p.ack]!)
          : deriveSentAck(p.farLeg, steps[p.ack]!)
  }))

  /** One insertion: the step, where it goes, and the near-leg index it mirrors. */
  interface Insertion {
    readonly before: number
    readonly step: StepDraft
    readonly mirrors: number
    readonly emits: boolean
    readonly auto: boolean
    readonly ts_us: number
  }
  const insertions: Array<Insertion> = []
  for (const r of resolved) {
    if (r.origin === "near-peer") {
      // The relayed INVITE lands where a relay of this emission would; the
      // answer goes out at the instant the caller took it.
      const relay = relayLatency(steps, timings, r.farLeg, steps[r.at.invite]!.leg, r.at.invite)
      insertions.push({
        before: r.at.answer,
        step: r.invite,
        mirrors: r.at.invite,
        emits: false,
        auto: false,
        ts_us: Math.min(timings[r.at.invite]!.ts_us + relay, timings[r.at.answer]!.ts_us)
      })
      insertions.push({
        before: r.at.answer,
        step: r.answer,
        mirrors: r.at.answer,
        emits: true,
        auto: false,
        ts_us: timings[r.at.answer]!.ts_us
      })
      if (r.ack !== undefined && r.at.ack !== undefined) {
        insertions.push({
          before: r.at.ack + 1,
          step: r.ack,
          mirrors: r.at.ack,
          emits: false,
          auto: true,
          ts_us: timings[r.at.ack]!.ts_us + relay
        })
      }
      continue
    }
    // The far party's INVITE goes out one relay hop before the near leg took
    // it, never before the step listed ahead of it; the 2xx lands one hop after
    // the near peer sent it; the ACK goes out one hop before the near leg took
    // it, never before the 2xx it acknowledges.
    const nearLeg = steps[r.at.invite]!.leg
    const relay = relayLatency(steps, timings, nearLeg, r.farLeg, r.at.invite)
    const answerAt = timings[r.at.answer]!.ts_us + relay
    insertions.push({
      before: r.at.invite,
      step: r.invite,
      mirrors: r.at.invite,
      emits: true,
      auto: false,
      ts_us: Math.max(
        timings[r.at.invite]!.ts_us - relay,
        r.at.invite === 0 ? 0 : timings[r.at.invite - 1]!.ts_us
      )
    })
    insertions.push({
      before: r.at.answer + 1,
      step: r.answer,
      mirrors: r.at.answer,
      emits: false,
      auto: false,
      ts_us: answerAt
    })
    if (r.ack !== undefined && r.at.ack !== undefined) {
      insertions.push({
        before: r.at.ack,
        step: r.ack,
        mirrors: r.at.ack,
        emits: true,
        auto: true,
        ts_us: Math.max(timings[r.at.ack]!.ts_us - relay, answerAt)
      })
    }
  }
  // Back to front, so every index is the pre-insertion one; equal positions
  // keep their listed order.
  insertions
    .map((ins, order) => ({ ins, order }))
    .sort((a, b) => b.ins.before - a.ins.before || b.order - a.order)
    .forEach(({ ins }) => {
      const near = sources[ins.mirrors]!
      const nearTiming = timings[ins.mirrors]!
      steps.splice(ins.before, 0, ins.step)
      timings.splice(ins.before, 0, {
        leg: ins.step.leg,
        emits: ins.emits,
        ts_us: ins.ts_us,
        typeKey: nearTiming.typeKey,
        cseq: nearTiming.cseq,
        timerLinked: false
      })
      sources.splice(ins.before, 0, {
        id: ins.step.id,
        pivotLeg: ins.step.leg,
        origLeg: near.origLeg,
        msgIdx: near.msgIdx,
        emits: ins.emits,
        auto: ins.auto,
        mirrored: true
      })
    })

  steps.forEach((step, i) => {
    step.id = stepId(i + 1)
  })
  sources.forEach((source, i) => {
    sources[i] = { ...source, id: stepId(i + 1) }
  })
  for (const r of resolved) {
    derived.push({
      farLeg: r.farLeg,
      origin: r.origin,
      ended: r.ended,
      nearInvite: r.nearInvite,
      nearAnswer: r.nearAnswer,
      ...(r.nearAck === undefined ? {} : { nearAck: r.nearAck }),
      invite: r.invite,
      answer: r.answer,
      ...(r.ack === undefined ? {} : { ack: r.ack })
    })
  }
  return { derived, left }
}

/**
 * The relay latency the capture measured on `arrivalLeg` for the other leg's
 * own emissions — the first arrival there, ahead of `before`, that relays an
 * `emitLeg` send — or 0 where the record holds none. A derived arrival lands
 * where a relay of THIS emission would, and a derived emission goes out where
 * a relay into the near leg's arrival must have started: the same hop the
 * captured messages crossed.
 */
const relayLatency = (
  steps: ReadonlyArray<StepDraft>,
  timings: ReadonlyArray<StepTiming>,
  arrivalLeg: string,
  emitLeg: string,
  before: number
): number => {
  for (let i = 0; i < before; i++) {
    if (steps[i]!.leg !== arrivalLeg || steps[i]!.op !== "expect") continue
    const origin = relayOriginOf(timings, i)
    if (origin < 0 || steps[origin]!.leg !== emitLeg) continue
    return Math.max(0, timings[i]!.ts_us - timings[origin]!.ts_us)
  }
  return 0
}

/** The placeholder every derived step carries until the flow is renumbered. */
const UNNUMBERED = "s?"

const placeholderDelay = (): StepDraft["delay"] => ({
  ms: 0,
  from: Tokens.anchorToken({ _tag: "trigger" }),
  compressible: true,
  timer_linked: false
})

/**
 * The far leg's `expect INVITE`: the near leg's request, compared by body
 * content where it carried one — the platform relays the offer — and by shape
 * where the document states the body as one. No frozen headers: the relaying
 * platform mints the request it puts on the far leg.
 */
const deriveInvite = (farLeg: string, near: StepDraft): StepDraft => ({
  id: UNNUMBERED,
  leg: farLeg,
  op: "expect",
  in_dialog: true,
  msg: { method: "INVITE", ...bodyExpected(near.msg.body) },
  delay: placeholderDelay(),
  ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
})

/**
 * The far leg's `send` of the 2xx the near leg took: status, reason, headers
 * and body exactly as the caller received them, since a relayed answer is the
 * far party's own; the comparison the expect stated is dropped, a send emits,
 * and so is its `retransmits` — a send's count is a ladder the peer is told to
 * run, and the near leg's count is the platform's.
 */
const deriveAnswer = (farLeg: string, near: StepDraft): StepDraft => {
  const msg: MsgSpecDraft = {
    status: near.msg.status!,
    ...(near.msg.reason === undefined ? {} : { reason: near.msg.reason }),
    "cseq-method": "INVITE",
    ...(near.msg.headers === undefined
      ? {}
      : { headers: near.msg.headers.map(({ name, value }) => ({ name, value })) }),
    ...(near.msg.body === undefined ? {} : { body: bodyEmitted(near.msg.body) })
  }
  return {
    id: UNNUMBERED,
    leg: farLeg,
    op: "send",
    in_dialog: true,
    msg,
    delay: placeholderDelay(),
    ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
  }
}

/**
 * The far leg's auto `expect ACK`, mirroring the near leg's: the near leg's
 * captured CSeq is its label (the transaction that obliged it is the other
 * leg's; nothing resolves the number), and a body it carried — a delayed
 * offer's answer — is compared by content, since the platform relays it end
 * to end. No `retransmits`: a count on the far leg is drawn from its own
 * final's ladder (§6.3), and the near leg's send count is an instruction to
 * the peer.
 */
const deriveAck = (farLeg: string, near: StepDraft): StepDraft => ({
  id: UNNUMBERED,
  leg: farLeg,
  op: "expect",
  auto: true,
  in_dialog: true,
  check: "record",
  msg: {
    method: "ACK",
    ...(near.msg.cseq === undefined ? {} : { cseq: near.msg.cseq }),
    ...bodyExpected(near.msg.body)
  },
  delay: placeholderDelay(),
  ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
})

/**
 * The far leg's `send INVITE` of the far party's re-INVITE: the offer and the
 * frozen headers as the near leg's expect took them, since a relayed request
 * is the far party's own. The dialog headers are the far leg's stack's, as on
 * every send. No `retransmits`: a send's count is a ladder the stack runs, and
 * the near leg's count is the platform's.
 */
const deriveSentInvite = (farLeg: string, near: StepDraft): StepDraft => ({
  id: UNNUMBERED,
  leg: farLeg,
  op: "send",
  in_dialog: true,
  msg: {
    method: "INVITE",
    ...(near.msg.headers === undefined
      ? {}
      : { headers: near.msg.headers.map(({ name, value }) => ({ name, value })) }),
    ...bodySent(near.msg.body)
  },
  delay: placeholderDelay(),
  ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
})

/**
 * The far leg's `expect` of the 2xx the near peer sent: status and reason as
 * sent, the body compared as the session description the near leg emitted,
 * no frozen headers — the relaying platform mints the response it puts on the
 * far leg. No `retransmits`: a count on the far leg is the platform's ladder
 * there, which the capture never held.
 */
const deriveExpectedAnswer = (farLeg: string, near: StepDraft): StepDraft => ({
  id: UNNUMBERED,
  leg: farLeg,
  op: "expect",
  in_dialog: true,
  msg: {
    status: near.msg.status!,
    ...(near.msg.reason === undefined ? {} : { reason: near.msg.reason }),
    "cseq-method": "INVITE",
    ...bodyExpected(near.msg.body)
  },
  delay: placeholderDelay(),
  ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
})

/**
 * The far leg's auto `send ACK`, mirroring the near leg's expect: the stack
 * composes it against the 2xx the far leg took, the near leg's captured CSeq is
 * its label as on the expect side, and a body the near leg's expect took — a
 * delayed offer's answer — is emitted as the far party's own.
 */
const deriveSentAck = (farLeg: string, near: StepDraft): StepDraft => ({
  id: UNNUMBERED,
  leg: farLeg,
  op: "send",
  auto: true,
  in_dialog: true,
  msg: {
    method: "ACK",
    ...(near.msg.cseq === undefined ? {} : { cseq: near.msg.cseq }),
    ...bodySent(near.msg.body)
  },
  delay: placeholderDelay(),
  ...(near.observed === undefined ? {} : { observed: { ...near.observed } })
})

/**
 * How an expect states the body a near-leg send carried: an SDP compared as a
 * session description under the send's own rewrite tokens, any other stored
 * body by content, a multipart by shape, and none as its own claim — the
 * readings `bodies.ts::expectBody` takes off the wire, taken off the step.
 */
const bodyExpected = (body: Body.Body | undefined): { readonly body: Body.Body } => {
  if (body === undefined) return { body: { mode: "absent" } }
  if (Body.isMultipartBody(body)) return { body: { mode: "multipart-present" } }
  if (Body.isResourceBody(body)) {
    const contentType = body["content-type"]
    const typed = contentType === undefined ? {} : { "content-type": contentType }
    return body.ref.endsWith(".sdp")
      ? { body: { ref: body.ref, rewrite: [...(body.rewrite ?? REWRITE)], ...typed, compare: "sdp" } }
      : { body: { ref: body.ref, ...(body.mode === undefined ? {} : { mode: body.mode }), ...typed } }
  }
  return { body }
}

/** How a send states the body a near-leg expect held: the resource, no comparison. */
const bodyEmitted = (body: Body.Body): Body.Body => {
  if (Body.isResourceBody(body)) {
    const { compare: _compare, ...emitted } = body
    return emitted
  }
  return body
}

/**
 * Whether a far-leg send can emit the body a near-leg expect took: none, or a
 * stored resource. A body the expect holds by shape only — a multipart, an
 * undeclared binary — has no bytes to emit.
 */
const emittable = (body: Body.Body | undefined): boolean =>
  body === undefined || Body.isResourceBody(body) || (Body.isShapeBody(body) && body.mode === "absent")

/** How a send states the body a near-leg expect took: the resource, no comparison; none for none. */
const bodySent = (body: Body.Body | undefined): { readonly body?: Body.Body } => {
  if (body === undefined || !Body.isResourceBody(body)) return {}
  return { body: bodyEmitted(body) }
}
