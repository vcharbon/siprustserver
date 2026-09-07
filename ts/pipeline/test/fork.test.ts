/**
 * A peer ringing one INVITE transaction under two To-tags rang two early
 * dialogs (RFC 3261 §12.1.1), and the cut names each one so a step says which
 * it rides — on the leg our UAS answers and on the leg it receives alike.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  CALLEE_URI,
  CALLER_URI,
  derivesOnePrefix,
  doc,
  leg,
  oneHop,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 }
]

const flowOf = (flows: Flows.FlowsDoc) =>
  synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())

/** The two dialogs the callee rang, as the capture named them. */
const FORK_A = "callee-fork-a"
const FORK_B = "callee-fork-b"
/** The two the platform published toward its caller for the same transaction. */
const RELAY_A = "sut-fork-a"
const RELAY_B = "sut-fork-b"

/** One side's two dialogs, named per leg: the caller-facing and the callee's. */
interface Dialogs {
  readonly relay: string
  readonly fork: string
}

/** Which dialogs a {@link forkedOnBothLegs} call rang and rejected under. */
interface Rang {
  /** The tags of the SECOND dialog each side rang — the first's, to fold it. */
  readonly second?: Dialogs
  /** The dialog each side's rejecting final and its ACK answered under. */
  readonly rejects?: Dialogs
}

/**
 * The smallest fork on BOTH legs: one branch per side, two To-tags on each, no
 * PRACK and no reroute. The transaction ends on a non-2xx and each side ACKs it
 * (RFC 3261 §17.1.1.3); which dialog rejects is the caller's to choose.
 */
const forkedOnBothLegs = ({
  second = { relay: RELAY_B, fork: FORK_B },
  rejects = { relay: RELAY_A, fork: FORK_A }
}: Rang = {}): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: RELAY_A }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 400, toTag: second.relay }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 503, reason: "Service Unavailable", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 900, toTag: rejects.relay }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 905, toTag: rejects.relay })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: FORK_A }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 390, toTag: second.fork }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 503, reason: "Service Unavailable", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 890, toTag: rejects.fork }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 895, toTag: rejects.fork })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/** The same call with one dialog: each side rang twice under one To-tag. */
const oneDialog = (): Flows.FlowsDoc =>
  forkedOnBothLegs({ second: { relay: RELAY_A, fork: FORK_A } })

/** The tag the caller wears for the whole call — {@link request}'s default. */
const CALLER_TAG = `from-${CALLER_CALL_ID}`

/**
 * One dialog, two CSeq spaces colliding on the number 1: the caller's own
 * INVITE, then a re-INVITE the platform sends back down the same leg numbered
 * from its own space (RFC 3261 §12.2). Two INVITE responses carry CSeq 1 under
 * two To-tags, and neither is a fork.
 */
const collidingCseqSpaces = (): Flows.FlowsDoc => {
  const { caller, sut } = SOCKETS
  const inDialog = { fromUri: CALLEE_URI, fromTag: RELAY_A, toTag: CALLER_TAG } as const
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: RELAY_A }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 900, toTag: RELAY_A }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 905, toTag: RELAY_A }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: caller, ts_ms: 5000, ruri: CALLER_URI, ...inDialog }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: caller, dst: sut, ts_ms: 5010, toUri: CALLER_URI, ...inDialog }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: sut, dst: caller, ts_ms: 5015, ruri: CALLER_URI, ...inDialog }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 9000, toTag: RELAY_A }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9005, toTag: RELAY_A })
      ])
    ],
    [{ legs: [0] }]
  )
}

const earlyOf = (steps: ReadonlyArray<{ id: string; early?: string }>, id: string) =>
  steps.find((s) => s.id === id)?.early

describe("early dialogs", () => {
  it("gives each To-tag of a forked transaction its own name, on both legs", () => {
    const flow = flowOf(forkedOnBothLegs())
    const named = flow.steps.filter((s) => s.early !== undefined)
    // Both legs fork, so both are named — and a leg's two dialogs never share.
    for (const l of ["A", "B"]) {
      const ids = new Set(named.filter((s) => s.leg === l).map((s) => s.early))
      expect(ids.size, `leg ${l} names two dialogs`).toBe(2)
    }
    // Ids are unique across the document: an id two legs declare names two
    // dialogs and no `${early:…}` accessor could resolve it.
    expect(new Set(named.map((s) => s.early)).size).toBe(4)
  })

  it("puts the rejecting final on the dialog that rang it, not on the other", () => {
    const flow = flowOf(forkedOnBothLegs())
    const on = (l: string, status: number, at: number) =>
      flow.steps.filter((s) => s.leg === l && s.msg.status === status)[at]!
    for (const l of ["A", "B"]) {
      const first = on(l, 180, 0)
      const second = on(l, 180, 1)
      expect(first.early).toBeDefined()
      expect(second.early).not.toBe(first.early)
      expect(on(l, 503, 0).early, `${l}'s 503 answers the dialog it rang under`)
        .toBe(first.early)
    }
  })

  it("names each leg's final from its own To-tag, where the two legs reject apart", () => {
    // `capture_178328`'s shape: the callee rejects under the SECOND dialog it
    // rang while the platform relays that final to the caller under the FIRST,
    // so a leg that copied its neighbour's name would get one of the two wrong.
    const flow = flowOf(forkedOnBothLegs({ rejects: { relay: RELAY_A, fork: FORK_B } }))
    const on = (l: string, status: number, at: number) =>
      flow.steps.filter((s) => s.leg === l && s.msg.status === status)[at]!
    expect(on("B", 503, 0).early, "the callee's 503 rides the second dialog it rang")
      .toBe(on("B", 180, 1).early)
    expect(on("A", 503, 0).early, "the caller's 503 rides the first dialog it saw")
      .toBe(on("A", 180, 0).early)
    const ack = flow.steps.find((s) => s.leg === "B" && s.msg.method === "ACK")!
    expect(ack.early, "the ACK answers the final's dialog").toBe(on("B", 503, 0).early)
  })

  it("names the ACK the fork it belongs to, and never an ACK the leg sends", () => {
    const flow = flowOf(forkedOnBothLegs())
    const acks = flow.steps.filter((s) => s.msg.method === "ACK")
    expect(acks.length).toBeGreaterThan(0)
    for (const ack of acks) {
      // An `early` on a request SEND is refused unless the method rides an
      // early dialog, which ACK does not: it belongs to the INVITE transaction.
      if (ack.op === "send") expect(ack.early, `${ack.id} sends the ACK`).toBeUndefined()
      else expect(ack.early, `${ack.id} expects the ACK`).toBe(earlyOf(flow.steps, "s5"))
    }
  })

  it("reports every dialog it named, with the tag the capture rang it under", () => {
    const flow = flowOf(forkedOnBothLegs())
    const flag = flow.flags.find((f) => f.kind === "early-dialogs-named")
    expect(flag?.detail).toContain(FORK_A)
    expect(flag?.detail).toContain(RELAY_B)
  })

  it("names nothing where a leg's two CSeq spaces collide on one number", () => {
    const flows = collidingCseqSpaces()
    const flow = synthesize(flows, build(flows, [{ leg: 0, hop: 0 }], sutSet(), plan(), derivesOnePrefix), plan())
    // The fixture holds the collision the key has to survive: the leg's INVITE
    // responses share CSeq 1 under two To-tags.
    const answers = flows.legs[0]!.msgs
      .map((m) => m.summary)
      .filter((sm) => sm.kind === "response" && sm.cseq.method === "INVITE" && sm.cseq.seq === 1)
      .map((sm) => sm.to.tag)
      .filter((tag) => tag !== null)
    expect(new Set(answers).size).toBe(2)
    expect(flow.steps.filter((s) => s.early !== undefined)).toEqual([])
    expect(flow.flags.find((f) => f.kind === "early-dialogs-named")).toBeUndefined()
  })

  it("names nothing where the transaction rang under one tag", () => {
    const flow = flowOf(oneDialog())
    expect(flow.steps.filter((s) => s.early !== undefined)).toEqual([])
    expect(flow.flags.find((f) => f.kind === "early-dialogs-named")).toBeUndefined()
  })
})
