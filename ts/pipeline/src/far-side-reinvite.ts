/**
 * The far side of an in-dialog INVITE exchange the capture holds on ONE leg
 * only, because the vantage lost the other leg past the answer it sent (§6.9).
 *
 * A leg whose captured record ENDS at a 2xx to INVITE its peer sends — no ACK,
 * not one datagram behind it — states nothing about the dialog past that
 * instant: not that it went quiet, only that the vantage stopped seeing it.
 * The near leg goes on: its peer sends an in-dialog INVITE and takes a 2xx
 * nothing on the far leg relays, so synthesis reads that answer as minted by
 * the platform. A platform that RELAYS such an INVITE end to end (the policy's
 * reading, `CasePolicy.relaysReinvite`) puts it on the far leg, where a
 * document scripting nothing for it can neither answer nor go on: the near
 * leg's expect waits for a 2xx nobody composes, and the run ends at its budget.
 *
 * So each such exchange is transcribed onto the far leg from the halves the
 * capture does hold: an `expect INVITE` mirroring the near leg's send, its body
 * compared by content where it carried one; a `send` of the 2xx the near leg
 * received, headers and body as captured, since a relayed answer is the far
 * party's own; and an auto `expect ACK` mirroring the near leg's, where the
 * capture holds one. Each derived step keeps the `observed` coordinate of the
 * near-leg message it copies — the platform relays that message, so it is what
 * the far-leg datagram is compared against — and its source is marked
 * `mirrored`, so a reader attributing a coordinate to a party reads past it.
 * `far-side-reinvite-derived` names every one.
 *
 * Bounded three ways, each ruling out a shape the capture does state. The far
 * leg's record must end at its 2xx: a leg the vantage kept watching that shows
 * no INVITE says the platform did NOT relay, which a replay must surface. The
 * near-leg answer must be a 2xx: a refusal the platform composed is its own,
 * and `far-side-reinvite-not-derived` says which exchange was left. And the
 * near-leg 2xx must have no relay origin: an answer the far leg's record does
 * hold is already a step.
 */
import { Body, Tokens } from "@sip/contracts"
import { relayOriginOf, type StepTiming } from "./delay.js"
import type { MsgSpecDraft, StepDraft } from "./draft.js"
import { stepId, type StepSource } from "./flowsteps.js"

/** One exchange transcribed onto the far leg, as step OBJECTS until ids settle. */
export interface FarSide {
  readonly farLeg: string
  /** The far leg's last captured step: the 2xx its record ends at. */
  readonly ended: StepDraft
  /** The near leg's own steps: the re-INVITE, its 2xx and its ACK. */
  readonly nearInvite: StepDraft
  readonly nearAnswer: StepDraft
  readonly nearAck?: StepDraft
  /** The derived steps, in document order. */
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
      if (s.leg !== nearLeg || s.op !== "send" || !isRequest(s, "INVITE")) continue
      const cseq = timings[i]!.cseq
      const answer = steps.findIndex(
        (a, j) =>
          j > i &&
          a.leg === nearLeg &&
          a.op === "expect" &&
          isFinalToInvite(a) &&
          timings[j]!.cseq === cseq
      )
      if (answer < 0) continue
      if (relayOriginOf(timings, answer) >= 0) continue
      if (!isSuccessToInvite(steps[answer]!)) {
        left.push({
          farLeg,
          nearInvite: s,
          detail:
            `the platform answered it ${steps[answer]!.msg.status} itself, which is its own ` +
            `refusal and no far party's`
        })
        continue
      }
      const ack = steps.findIndex(
        (a, j) =>
          j > answer &&
          a.leg === nearLeg &&
          a.op === "send" &&
          isRequest(a, "ACK") &&
          timings[j]!.cseq === cseq
      )
      planned.push({ farLeg, ended, invite: i, answer, ack: ack < 0 ? undefined : ack })
    }
  }
  if (planned.length === 0) return { derived, left }

  // Resolved to objects before any splice moves an index.
  const resolved = planned.map((p) => ({
    farLeg: p.farLeg,
    ended: steps[p.ended]!,
    nearInvite: steps[p.invite]!,
    nearAnswer: steps[p.answer]!,
    nearAck: p.ack === undefined ? undefined : steps[p.ack]!,
    at: p,
    invite: deriveInvite(p.farLeg, steps[p.invite]!),
    answer: deriveAnswer(p.farLeg, steps[p.answer]!),
    ack: p.ack === undefined ? undefined : deriveAck(p.farLeg, steps[p.ack]!)
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
    // The relayed INVITE lands where a relay of this emission would; the
    // answer goes out at the instant the caller took it.
    const relay = relayLatency(steps, timings, r.farLeg, r.at.invite)
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
 * The relay latency the capture measured on `farLeg` for the near leg's own
 * emissions — the first arrival there that relays a near-leg send — or 0 where
 * the record holds none. A derived arrival lands where a relay of THIS emission
 * would, which is the same hop the initial INVITE crossed.
 */
const relayLatency = (
  steps: ReadonlyArray<StepDraft>,
  timings: ReadonlyArray<StepTiming>,
  farLeg: string,
  nearInvite: number
): number => {
  const nearLeg = steps[nearInvite]!.leg
  for (let i = 0; i < nearInvite; i++) {
    if (steps[i]!.leg !== farLeg || steps[i]!.op !== "expect") continue
    const origin = relayOriginOf(timings, i)
    if (origin < 0 || steps[origin]!.leg !== nearLeg) continue
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
 * far party's own; the comparison the expect stated is dropped, a send emits.
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
 * The far leg's auto `expect ACK`, mirroring the near leg's: the same captured
 * CSeq pairs it, and a body it carried — a delayed offer's answer — is compared
 * by content, since the platform relays it end to end.
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
