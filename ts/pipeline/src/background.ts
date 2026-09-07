/**
 * Background claims over a capture (`PCAP2TEST_PIVOT_V3.md` §5.1): which
 * observations a stated background policy collapses out of flow synthesis.
 *
 * A policy an actor states does double duty. The assembled actor carries it, so
 * the replay answers the replaying SUT's own traffic of that method; and the
 * captured platform's own locally-minted exchanges of that method collapse into
 * it instead of becoming steps — the cadence is a system parameter of the
 * platform that ran it, never call behaviour, so scripting it would gate the
 * flow on another deployment's liveness timer.
 *
 * Claimed per exchange, narrowly: an in-dialog request of a policy-answered
 * method that arrives at the actor WITHOUT a cross-leg origin (no captured
 * cross-leg emit of the same type within the relay proximity window, and an
 * emit origins only the FIRST same-type arrival that follows it) was minted
 * by the captured platform; the claim takes that request, the answers the actor
 * returned on the same CSeq, and every retransmission of either. A relayed
 * (end-to-end) request keeps its steps, and so does an out-of-dialog probe or a
 * request the ACTOR emits toward the platform: those are call behaviour whose
 * divergence a replay must surface. An endpoint only ever receives a response
 * of the method by having emitted such a request, so the answers of a
 * non-relayed exchange cannot be another leg's relay origin.
 *
 * A vantage that lost the request datagram and kept the answer is claimed on the
 * answer alone: an in-dialog response of a policy-answered method that the actor
 * EMITTED and holds no arriving request of that CSeq for was minted by the same
 * cadence as its captured neighbours, and scripting it would ask a leg to answer
 * a transaction it never receives.
 */
import type { Flows, Placement } from "@sip/contracts"
import { typeKey } from "./msgspec.js"

/** Actor id -> the background policies that actor states. */
export type BackgroundMap = ReadonlyMap<string, ReadonlyArray<Placement.BackgroundPolicy>>

/** One observation, as synthesis walks them (time-sorted). */
export interface BackgroundObs {
  readonly actorId: string
  readonly pivotLeg: string
  readonly emits: boolean
  readonly origLeg: number
  readonly msgIdx: number
  readonly ts_us: number
}

/** Mirrors `delay.ts`: a relay is instantaneous within this window (µs). */
const PROXIMITY_US = 2_000_000

const coord = (origLeg: number, msgIdx: number): string => `${origLeg}:${msgIdx}`

const requestMethod = (m: Flows.Msg): string | undefined =>
  m.summary.kind === "request" ? m.summary.method.toUpperCase() : undefined

/**
 * The `origLeg:msgIdx` coordinates background policies claim out of synthesis.
 * Empty when no actor states a policy — the zero-cost common case.
 */
export const claimedByBackground = (
  flows: Flows.FlowsDoc,
  obs: ReadonlyArray<BackgroundObs>,
  background: BackgroundMap
): Set<string> => {
  const claimed = new Set<string>()
  if (background.size === 0) return claimed
  const msgOf = (o: BackgroundObs): Flows.Msg => flows.legs[o.origLeg]!.msgs[o.msgIdx]!

  for (const o of obs) {
    if (o.emits) continue
    const msg = msgOf(o)
    const method = requestMethod(msg)
    if (method === undefined) continue
    const answers = background.get(o.actorId) ?? []
    if (!answers.some((p) => p.match.method.toUpperCase() === method)) continue
    // An out-of-dialog probe is not the platform's in-dialog audit.
    if (msg.summary.to.tag === null) continue
    // Retransmissions ride their root's claim, never their own.
    if (msg.repeat_of !== undefined || msg.retx) continue
    // An emit is the origin of the FIRST same-type arrival that follows it and
    // of no other: a platform whose own audit cadence lands inside the window a
    // genuinely relayed exchange already used would otherwise borrow that
    // exchange's origin and be scripted as call behaviour.
    const origin = obs.find(
      (c) =>
        c.emits &&
        c.pivotLeg !== o.pivotLeg &&
        c.ts_us <= o.ts_us &&
        o.ts_us - c.ts_us < PROXIMITY_US &&
        typeKey(msgOf(c)) === typeKey(msg) &&
        !obs.some((earlier) => {
          if (earlier.emits || earlier.pivotLeg === c.pivotLeg) return false
          if (earlier.ts_us < c.ts_us || earlier.ts_us >= o.ts_us) return false
          const em = msgOf(earlier)
          return em.repeat_of === undefined && !em.retx && typeKey(em) === typeKey(msg)
        })
    )
    if (origin !== undefined) continue
    claimed.add(coord(o.origLeg, o.msgIdx))
    // The actor's answers on the same CSeq die at the hop the request came from.
    for (const w of obs) {
      if (w.origLeg !== o.origLeg || !w.emits || w.ts_us < o.ts_us) continue
      const wm = msgOf(w)
      if (
        wm.summary.kind === "response" &&
        wm.summary.cseq.method.toUpperCase() === method &&
        wm.summary.cseq.seq === msg.summary.cseq.seq
      ) {
        claimed.add(coord(w.origLeg, w.msgIdx))
      }
    }
  }

  // An answer whose request the vantage lost: same actor, same policy-answered
  // method, in-dialog, and no arriving request on the leg to pair it with.
  for (const o of obs) {
    if (!o.emits) continue
    const msg = msgOf(o)
    if (msg.summary.kind !== "response") continue
    if (msg.summary.to.tag === null) continue
    if (claimed.has(coord(o.origLeg, o.msgIdx))) continue
    const method = msg.summary.cseq.method.toUpperCase()
    const answers = background.get(o.actorId) ?? []
    if (!answers.some((p) => p.match.method.toUpperCase() === method)) continue
    const paired = obs.some((r) => {
      if (r.emits || r.origLeg !== o.origLeg || r.ts_us > o.ts_us) return false
      const rm = msgOf(r)
      return (
        rm.summary.kind === "request" &&
        rm.summary.method.toUpperCase() === method &&
        rm.summary.cseq.seq === msg.summary.cseq.seq
      )
    })
    if (!paired) claimed.add(coord(o.origLeg, o.msgIdx))
  }

  if (claimed.size === 0) return claimed

  // Retransmissions of a claimed message: `repeat_of` where the document
  // states it, the legacy retx+type+CSeq match where it does not.
  for (const o of obs) {
    const msg = msgOf(o)
    if (msg.repeat_of !== undefined) {
      if (claimed.has(coord(o.origLeg, msg.repeat_of))) claimed.add(coord(o.origLeg, o.msgIdx))
      continue
    }
    if (!msg.retx) continue
    const twin = obs.some((r) => {
      if (r.origLeg !== o.origLeg || !claimed.has(coord(r.origLeg, r.msgIdx))) return false
      const rm = msgOf(r)
      return typeKey(rm) === typeKey(msg) && rm.summary.cseq.seq === msg.summary.cseq.seq
    })
    if (twin) claimed.add(coord(o.origLeg, o.msgIdx))
  }
  return claimed
}

export * as Background from "./background.js"
