/**
 * Where an emitted message sits in its call and which received message it
 * relays, over hand-made calls whose answer is known: A calls, the system
 * dials B, and later legs are hunted, resources or transfers.
 */
import { describe, expect, it } from "vitest"
import { type Observation, place, type Placed } from "../src/situation.js"

const req = (leg: string, emitted: boolean, method: string, inDialog = true): Observation => ({
  leg,
  emitted,
  request: { method, dialogCreating: !inDialog }
})
const res = (leg: string, emitted: boolean, status: number, method = "INVITE"): Observation => ({
  leg,
  emitted,
  response: { status, method }
})

/** B answers A: caller leg A, dialled leg B, one keepalive, B hangs up. */
const basicCall: ReadonlyArray<Observation> = [
  req("A", false, "INVITE", false),
  res("A", true, 100),
  req("B", true, "INVITE", false),
  res("B", false, 100),
  res("B", false, 180),
  res("A", true, 180),
  res("B", false, 200),
  res("A", true, 200),
  req("A", false, "ACK"),
  req("B", true, "ACK"),
  req("A", true, "OPTIONS"),
  res("A", false, 200, "OPTIONS"),
  req("B", false, "BYE"),
  res("B", true, 200, "BYE"),
  req("A", true, "BYE"),
  res("A", false, 200, "BYE")
]

describe("situation and pairing", () => {
  const placed = place(basicCall, () => "A")
  const at = (i: number) => placed[i]

  it("leaves received messages unplaced", () => {
    expect(at(0)).toBeUndefined()
    expect(at(4)).toBeUndefined()
  })

  it("places the launched INVITE and pairs it with the caller's INVITE", () => {
    expect(at(2)).toEqual({ situation: "INV", pair: 0 })
  })

  it("never pairs a 100 Trying", () => {
    expect(at(1)).toEqual({ situation: "T100", pair: undefined })
  })

  it("pairs a relayed 18x and final with the same status received on the other leg", () => {
    expect(at(5)).toEqual({ situation: "P18", pair: 4 })
    expect(at(7)).toEqual({ situation: "FIN:2xx", pair: 6 })
  })

  it("pairs a relayed in-dialog request with the same method received on the other leg", () => {
    expect(at(9)).toEqual({ situation: "DQ:ACK", pair: 8 })
    expect(at(14)).toEqual({ situation: "DQ:BYE", pair: 12 })
  })

  it("names an unpaired in-dialog request system-originated", () => {
    expect(at(10)).toEqual({ situation: "SO:OPTIONS", pair: undefined })
  })

  it("leaves a locally answered response unpaired and says so", () => {
    expect(at(13)).toEqual({ situation: "DS:2xx:BYE:local", pair: undefined })
  })

  it("names the ACK of a non-2xx hop by hop, and pairs the relayed failure", () => {
    const failed = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        res("B", false, 486),
        req("B", true, "ACK"),
        res("A", true, 486)
      ],
      () => "A"
    )
    expect(failed[3]).toEqual({ situation: "ACK-ERR", pair: undefined })
    expect(failed[4]).toEqual({ situation: "FIN:4xx", pair: 2 })
  })

  it("never pairs a later leg's ACK with an ACK received before its final", () => {
    const transfer = place(
      [
        ...basicCall.slice(0, 10),
        req("C", true, "INVITE", false),
        res("C", false, 200),
        req("C", true, "ACK")
      ],
      () => "A"
    )
    expect(transfer[12]).toEqual({ situation: "SO:ACK", pair: undefined })
  })

  it("pairs a relayed request that is the first message seen on its leg", () => {
    const midCall = place([req("27", false, "BYE"), res("27", true, 200, "BYE"), req("28", true, "BYE")], () => "27")
    expect(midCall[2]).toEqual({ situation: "DQ:BYE", pair: 0 })
  })

  it("pairs a relayed response only with the last message received on another leg", () => {
    const released = place(
      [
        req("A", false, "INVITE", false),
        req("M", true, "INVITE", false),
        res("M", false, 200),
        req("M", true, "ACK"),
        req("B", true, "INVITE", false),
        res("B", false, 180),
        req("M", true, "BYE"),
        res("M", false, 200, "BYE"),
        res("A", true, 200)
      ],
      () => "A"
    )
    expect(released[8]).toEqual({ situation: "FIN:2xx:local", pair: undefined })
  })

  it("pairs relayed responses in arrival order when several arrived before the first relay", () => {
    const queued = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        res("B", false, 183),
        res("B", false, 183),
        res("A", true, 183),
        res("A", true, 183)
      ],
      () => "A"
    )
    expect(queued[4]).toEqual({ situation: "P18", pair: 2 })
    expect(queued[5]).toEqual({ situation: "P18", pair: 3 })
  })

  it("pairs a relayed response with the one whose body it carries when the system absorbed another", () => {
    const sdp = (o: Observation, carries: boolean): Observation => ({
      leg: o.leg,
      emitted: o.emitted,
      response: { ...o.response!, sdp: carries }
    })
    const absorbed = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        sdp(res("B", false, 183), false),
        sdp(res("B", false, 183), true),
        sdp(res("A", true, 183), true)
      ],
      () => "A"
    )
    expect(absorbed[4]).toEqual({ situation: "P18", pair: 3 })
    const both = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        sdp(res("B", false, 183), true),
        sdp(res("B", false, 183), false),
        sdp(res("A", true, 183), true),
        sdp(res("A", true, 183), false)
      ],
      () => "A"
    )
    expect(both[4]).toEqual({ situation: "P18", pair: 2 })
    expect(both[5]).toEqual({ situation: "P18", pair: 3 })
  })

  it("pairs a hunt's relayed failure with the last leg's, not an earlier leg's of the same status", () => {
    const hunted = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        res("B", false, 486),
        req("B", true, "ACK"),
        req("C", true, "INVITE", false),
        res("C", false, 486),
        req("C", true, "ACK"),
        res("A", true, 486)
      ],
      () => "A"
    )
    expect(hunted[7]).toEqual({ situation: "FIN:4xx", pair: 5 })
  })

  it("pairs a repeated relay with the response it repeats", () => {
    const repeated = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        res("B", false, 183),
        res("A", true, 183),
        res("A", true, 183)
      ],
      () => "A"
    )
    expect(repeated[3]).toEqual({ situation: "P18", pair: 2 })
    expect(repeated[4]).toEqual({ situation: "P18", pair: 2 })
  })

  it("matches a response to the request with its CSeq", () => {
    const cseq = (o: Observation, n: number): Observation =>
      o.request !== undefined
        ? { leg: o.leg, emitted: o.emitted, request: { ...o.request, cseq: n } }
        : { leg: o.leg, emitted: o.emitted, response: { ...o.response!, cseq: n } }
    const crossed = place(
      [
        cseq(req("A", false, "INVITE", false), 1),
        cseq(req("A", false, "INVITE"), 2),
        cseq(res("A", true, 200), 1),
        cseq(res("A", true, 200), 2)
      ],
      () => undefined
    )
    expect(crossed[2]?.situation).toBe("FIN:2xx:local")
    expect(crossed[3]?.situation).toBe("DS:2xx:INVITE:local")
  })

  it("marks a final the system minted itself local", () => {
    const minted = place([req("A", false, "INVITE", false), req("B", true, "INVITE", false), res("A", true, 480)], () => "A")
    expect(minted[2]).toEqual({ situation: "FIN:4xx:local", pair: undefined })
  })

  it("keeps CANCEL in-dialog-request shaped and its answer a DS", () => {
    const cancelled = place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        req("A", false, "CANCEL", false),
        res("A", true, 200, "CANCEL"),
        req("B", true, "CANCEL", false)
      ],
      () => "A"
    )
    expect(cancelled[3]).toEqual({ situation: "DS:2xx:CANCEL:local", pair: undefined })
    expect(cancelled[4]).toEqual({ situation: "DQ:CANCEL", pair: 2 })
  })

  /** A's CANCEL relayed to B, B's `status` to that CANCEL received, then the system's `status` to A's CANCEL. */
  const answeredAfterTheFarEnd = (status: number): ReadonlyArray<Placed | undefined> =>
    place(
      [
        req("A", false, "INVITE", false),
        req("B", true, "INVITE", false),
        req("A", false, "CANCEL", false),
        req("B", true, "CANCEL", false),
        res("B", false, status, "CANCEL"),
        res("A", true, status, "CANCEL")
      ],
      () => "A"
    )

  it("answers a CANCEL hop by hop: its 200 never relays the far end's 200 to the system's CANCEL", () => {
    const placed = answeredAfterTheFarEnd(200)
    expect(placed[3]).toEqual({ situation: "DQ:CANCEL", pair: 2 })
    expect(placed[5]).toEqual({ situation: "DS:2xx:CANCEL:local", pair: undefined })
  })

  it("answers a CANCEL hop by hop: its 481 never relays the far end's 481 to the system's CANCEL", () => {
    expect(answeredAfterTheFarEnd(481)[5]).toEqual({ situation: "DS:4xx:CANCEL:local", pair: undefined })
  })

  it("pairs a 200 to BYE emitted after the far end's 200 to the relayed BYE", () => {
    const relayed = place(
      [
        ...basicCall.slice(0, 10),
        req("A", false, "BYE"),
        req("B", true, "BYE"),
        res("B", false, 200, "BYE"),
        res("A", true, 200, "BYE")
      ],
      () => "A"
    )
    expect(relayed[13]).toEqual({ situation: "DS:2xx:BYE", pair: 12 })
  })
})
