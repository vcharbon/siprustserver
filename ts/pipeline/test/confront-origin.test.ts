/**
 * The session origin across one leg of a verbatim run: the `o=` row of a
 * description whose session the replayed endpoint opened is marked as a
 * minted origin where the replay keeps the capture's sess-id pairing and
 * version steps; a session continued under another sess-id and a relayed
 * peer's origin with shifted numbers stay unmarked rows. A rebooked run masks
 * the origin numbers and reads no ledger.
 */
import type { Body, Bundle, Flow, Pivot } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { confront } from "../src/confront.js"
import type { BodyProbe } from "../src/probe.js"
import { signature } from "../src/probe.js"

const sdp = (origin: string): string =>
  `v=0\r\n${origin}\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 6000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n`
const o = (id: number, version: number, user = "-", address = "192.0.2.10"): string =>
  `o=${user} ${id} ${version} IN IP4 ${address}`

const utf8 = new TextEncoder()
const delay = { ms: 0, from: "trigger" as Flow.Delay["from"], compressible: true, timer_linked: false }

const sdpBody = (ref: string): Body.Body => ({ ref, compare: "sdp", "content-type": "application/sdp" } as Body.Body)

const step = (id: string, leg: string, op: "send" | "expect", msg: Flow.Step["msg"], inDialog = true): Flow.Step => ({
  id,
  leg,
  op,
  in_dialog: inDialog,
  check: "record",
  msg,
  delay
})

/** The offer the endpoint opens toward leg B, then the ACK that continues it. */
const flow: ReadonlyArray<Flow.Step> = [
  step("s1", "A", "send", { method: "INVITE", body: sdpBody("resources/a.sdp") }, false),
  step("s2", "B", "expect", { method: "INVITE", body: sdpBody("resources/b1.sdp") }, false),
  step("s3", "B", "expect", { method: "ACK", body: sdpBody("resources/b2.sdp") })
]

const pivot: Pivot.PivotV3 = {
  pivot_version: 3,
  case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
  identities: [],
  calls: [],
  endpoints: [],
  actors: [],
  legs: [],
  flow: [...flow],
  timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
}

const verdict: Bundle.RunVerdict = { case: "c", lane: "fake", status: "ok", failures: [] }

const PEER = o(555, 555, "peer", "198.51.100.7")

const message = (
  seq: number,
  dir: "in" | "out",
  stepId: string,
  startLine: string,
  cseq: string,
  toTag: boolean,
  body: string
): Bundle.RecordedMessage => {
  const bytes = utf8.encode(body)
  const head = [
    startLine,
    `To: <sip:b@example.invalid>${toTag ? ";tag=b" : ""}`,
    `CSeq: ${cseq}`,
    "Content-Type: application/sdp",
    `Content-Length: ${bytes.length}`
  ].join("\r\n")
  return {
    seq,
    dir,
    at_us: seq * 1000,
    step: stepId,
    raw: `${head}\r\n\r\n${body}`,
    body: { content_type: "application/sdp", len: bytes.length }
  } as Bundle.RecordedMessage
}

const run = (
  replayedOffer: string,
  replayedAck: string,
  captured: { readonly offer: string; readonly ack: string } = { offer: sdp(o(1000, 1000)), ack: sdp(o(1000, 1001)) },
  media: Bundle.MediaMode = "verbatim"
): ReadonlyArray<BodyProbe> =>
  confront({
    pivot,
    verdict,
    recordings: new Map([
      ["A", [message(1, "out", "s1", "INVITE sip:b@example.invalid SIP/2.0", "1 INVITE", false, sdp(PEER))]],
      [
        "B",
        [
          message(2, "in", "s2", "INVITE sip:b@example.invalid SIP/2.0", "1 INVITE", false, replayedOffer),
          message(3, "in", "s3", "ACK sip:b@example.invalid SIP/2.0", "1 ACK", true, replayedAck)
        ]
      ]
    ]),
    resources: new Map([
      ["resources/a.sdp", utf8.encode(sdp(PEER))],
      ["resources/b1.sdp", utf8.encode(captured.offer)],
      ["resources/b2.sdp", utf8.encode(captured.ack)]
    ]),
    media
  }).probes.flatMap((p) => (p.probe.kind === "body" ? [p.probe] : []))

/**
 * A cell with a second leg A that also receives the endpoint's session: the
 * INVITE on B opens it, a re-INVITE on A carries what each side sent there.
 */
const acrossLegs = (
  captured: { readonly b: string; readonly a: string },
  replayed: { readonly b: string; readonly a: string }
): ReadonlyArray<BodyProbe> => {
  const flowAB: ReadonlyArray<Flow.Step> = [
    step("s2", "B", "expect", { method: "INVITE", body: sdpBody("resources/b1.sdp") }, false),
    step("s4", "A", "expect", { method: "INVITE", body: sdpBody("resources/a2.sdp") })
  ]
  return confront({
    pivot: { ...pivot, flow: [...flowAB] },
    verdict,
    recordings: new Map([
      ["A", [message(4, "in", "s4", "INVITE sip:a@example.invalid SIP/2.0", "2 INVITE", true, replayed.a)]],
      ["B", [message(2, "in", "s2", "INVITE sip:b@example.invalid SIP/2.0", "1 INVITE", false, replayed.b)]]
    ]),
    resources: new Map([
      ["resources/b1.sdp", utf8.encode(captured.b)],
      ["resources/a2.sdp", utf8.encode(captured.a)]
    ]),
    media: "verbatim"
  }).probes.flatMap((p) => (p.probe.kind === "body" ? [p.probe] : []))
}

const rows = (probes: ReadonlyArray<BodyProbe>) =>
  probes.map((p) => ({ step: p.step, signature: signature(p), minted: p.sdp?.mintedOrigin === true }))

describe("the origin of a session the endpoint opens", () => {
  it("the opened offer and its +1 continuation are marked `session:o=` rows", () => {
    expect(rows(run(sdp(o(7, 7)), sdp(o(7, 8))))).toEqual([
      { step: "s2", signature: "body:sdp:session:o=:initial-invite", minted: true },
      { step: "s3", signature: "body:sdp:session:o=:request:ACK:in-dialog", minted: true }
    ])
  })

  it("the continuation under another sess-id at the right +1 step is an unmarked row", () => {
    expect(rows(run(sdp(o(7, 7)), sdp(o(9, 8))))).toEqual([
      { step: "s2", signature: "body:sdp:session:o=:initial-invite", minted: true },
      { step: "s3", signature: "body:sdp:session:o=:request:ACK:in-dialog", minted: false }
    ])
  })

  it("a continuation at another version step is an unmarked row", () => {
    expect(rows(run(sdp(o(7, 7)), sdp(o(7, 9))))[1]).toMatchObject({ minted: false })
  })

  it("a relayed peer origin with shifted numbers is an unmarked row", () => {
    const shifted = sdp(o(556, 556, "peer", "198.51.100.7"))
    expect(rows(run(shifted, shifted, { offer: sdp(PEER), ack: sdp(PEER) }))).toEqual([
      { step: "s2", signature: "body:sdp:session:o=:initial-invite", minted: false },
      { step: "s3", signature: "body:sdp:session:o=:request:ACK:in-dialog", minted: false }
    ])
  })

  it("where the capture minted and the replay relays the peer's origin, the row is unmarked", () => {
    expect(rows(run(sdp(PEER), sdp(o(556, 556, "peer", "198.51.100.7"))))[0]).toMatchObject({ minted: false })
  })

  it("a session the capture carries onto another leg, which the replay opens afresh there, is unmarked", () => {
    const got = rows(acrossLegs(
      { b: sdp(o(1000, 1000)), a: sdp(o(1000, 1001)) },
      { b: sdp(o(7, 7)), a: sdp(o(9, 9)) }
    ))
    expect(got).toEqual([
      { step: "s4", signature: "body:sdp:session:o=:request:INVITE:in-dialog", minted: false },
      { step: "s2", signature: "body:sdp:session:o=:initial-invite", minted: true }
    ])
  })

  it("two sessions the capture keeps apart per leg, which the replay shares at +1, are unmarked on the second", () => {
    const got = rows(acrossLegs(
      { b: sdp(o(1000, 1000)), a: sdp(o(2000, 2000)) },
      { b: sdp(o(7, 7)), a: sdp(o(7, 8)) }
    ))
    expect(got.find((r) => r.step === "s4")).toMatchObject({ minted: false })
  })

  it("a session carried across legs alike on both sides stays marked", () => {
    const got = rows(acrossLegs(
      { b: sdp(o(1000, 1000)), a: sdp(o(1000, 1001)) },
      { b: sdp(o(7, 7)), a: sdp(o(7, 8)) }
    ))
    expect(got.every((r) => r.minted)).toBe(true)
  })

  it("the same bytes state nothing", () => {
    expect(run(sdp(o(1000, 1000)), sdp(o(1000, 1001)))).toEqual([])
  })

  it("a rebooked run masks the origin numbers and states nothing for them", () => {
    expect(run(sdp(o(7, 7)), sdp(o(9, 8)), undefined, "rebooked")).toEqual([])
  })
})
