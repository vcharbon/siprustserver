/**
 * The RFC charge read back to whoever authored it: a deviation the SUT only
 * transported is a peer's, and one it composed itself is not — the whole rule
 * being what it refuses.
 */
import type { Bundle } from "@sip/contracts"
import { Wire } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import * as RfcInherit from "../src/rfc-inherit.js"

const OFFENDING = `v=0\r\no=- 1 1 IN IP4 198.51.100.1\r\ns=-\r\nm=image 0 udptl\r\n`
const COMPOSED = `v=0\r\no=- 2 2 IN IP4 198.51.100.2\r\ns=-\r\nm=image 0 udptl\r\n`

/** The party a detail opens on: the SUT on its own leg's call, the scripted peer on the peer's. */
const partyOf = (callId: string): string => (callId.startsWith("b-") ? "203.0.113.1:5080" : "203.0.113.1:5060")

/** What the audit wrote about one deviation. */
const finding = (over: Partial<Bundle.RfcFinding>): Bundle.RfcFinding => ({
  rule: "answer-media-type-matches-offer",
  lane: "203.0.113.1:5080",
  detail:
    `Answer media type does not match the offer's at 1 stream position(s) — ` +
    `m=[0] offered 'audio', answered 'image' (callId b-1@203.0.113.1, CSeq 1) — RFC 3264 §6.1`,
  advisory: false,
  gating: true,
  offending: 45,
  charged: "203.0.113.1:5080",
  ...over
})

/** The same deviation as the audit reported it against the scripted peer's own emission. */
const peer = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
  finding({
    lane: "203.0.113.1:5060",
    detail:
      `Answer media type does not match the offer's at 1 stream position(s) — ` +
      `m=[0] offered 'audio', answered 'image' (callId A-9@pivot.invalid, CSeq 6) — RFC 3264 §6.1`,
    gating: false,
    charged: "203.0.113.1:5060",
    actor: "ep0",
    ...over
  })

const message = (
  dir: Bundle.Dir,
  callId: string,
  cseq: number,
  body: string
): Bundle.RecordedMessage => ({
  seq: 1,
  dir,
  at_us: 0,
  raw:
    `SIP/2.0 200 OK\r\nCall-ID: ${callId}\r\nCSeq: ${cseq} INVITE\r\n` +
    `Content-Type: application/sdp\r\nContent-Length: ${body.length}\r\n\r\n${body}`
})

/** A bodiless response as a leg recorded it, stamped on the run clock. */
const response = (
  dir: Bundle.Dir,
  at_us: number,
  status: string,
  callId: string,
  cseq: string,
  branch: string
): Bundle.RecordedMessage => ({
  seq: 1,
  dir,
  at_us,
  raw:
    `SIP/2.0 ${status}\r\nVia: SIP/2.0/UDP 203.0.113.1:5060;branch=${branch}\r\n` +
    `Call-ID: ${callId}\r\nCSeq: ${cseq}\r\nContent-Length: 0\r\n\r\n`
})

/** What the audit wrote of a 421 with no Require, charged to the SUT's relay. */
const missing421 = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
  finding({
    rule: "unsupported-extension-421",
    detail:
      `203.0.113.1:5080: Sent 421 response (callId b-1@203.0.113.1, branch z9hG4bK-B-1) lacks Require header ` +
      `listing required extensions — {Uas} RFC 3261 §21.4.15 / RFC3261-MUST-182`,
    offending: 7,
    ...over
  })

/** The same 421 as the audit reported it against the scripted peer's own emission. */
const peer421 = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
  missing421({
    lane: "203.0.113.1:5060",
    detail:
      `203.0.113.1:5060: Sent 421 response (callId A-9@pivot.invalid, branch z9hG4bKf91b) lacks Require header ` +
      `listing required extensions — {Uas} RFC 3261 §21.4.15 / RFC3261-MUST-182`,
    gating: false,
    charged: "203.0.113.1:5060",
    actor: "ep0",
    offending: 5,
    ...over
  })

/** What the audit wrote of a 415 with no Accept, charged to the SUT's relay. */
const missing415 = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
  finding({
    rule: "unsupported-415-accepts",
    detail:
      `203.0.113.1:5080: Sent 415 response (callId b-1@203.0.113.1, branch z9hG4bK-B-3) carries no Accept / ` +
      `Accept-Encoding / Accept-Language header — {Uas} RFC 3261 §8.2.3 / RFC3261-MUST-036`,
    offending: 42,
    ...over
  })

/** The same 415 as the audit reported it against the scripted peer's own emission. */
const peer415 = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
  missing415({
    lane: "203.0.113.1:5060",
    detail:
      `203.0.113.1:5060: Sent 415 response (callId A-9@pivot.invalid, branch z9hG4bKb78b) carries no Accept / ` +
      `Accept-Encoding / Accept-Language header — {Uas} RFC 3261 §8.2.3 / RFC3261-MUST-036`,
    gating: false,
    charged: "203.0.113.1:5060",
    actor: "ep0",
    offending: 39,
    ...over
  })

/** The peer put the body on the wire; the SUT handed the same bytes to the far leg. */
const relayed: ReadonlyArray<Bundle.RecordedMessage> = [
  message("out", "A-9@pivot.invalid", 6, OFFENDING),
  message("in", "b-1@203.0.113.1", 1, OFFENDING)
]

describe("the RFC charge read back to its author", () => {
  it("reads a charge back to the peer that committed it and relayed the bytes", () => {
    const got = RfcInherit.ruling([finding({}), peer()], relayed)
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["answer-media-type-matches-offer"])
    expect(got.charged).toEqual([])
  })

  /**
   * A relayed body that is not UTF-8 is recorded as `head` + `body_b64`; the
   * reader compares the bytes both lines carry, in whichever arm they came.
   */
  it("reads a charge back to the peer over bodies recorded as bytes", () => {
    const blob = Buffer.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00])
    const head = (callId: string, cseq: number): string =>
      `SIP/2.0 200 OK\r\nCall-ID: ${callId}\r\nCSeq: ${cseq} INVITE\r\n` +
      `Content-Type: application/vnd.example.blob\r\nContent-Length: ${blob.length}\r\n\r\n`
    const bytes = (dir: Bundle.Dir, callId: string, cseq: number, body: Buffer): Bundle.RecordedMessage =>
      ({ seq: 1, dir, at_us: 0, head: head(callId, cseq), body_b64: body.toString("base64") }) as Bundle.RecordedMessage
    const relayedBytes = [bytes("out", "A-9@pivot.invalid", 6, blob), bytes("in", "b-1@203.0.113.1", 1, blob)]
    expect(RfcInherit.ruling([finding({}), peer()], relayedBytes).charged).toEqual([])
    const other = Buffer.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x01])
    const composedBytes = [bytes("out", "A-9@pivot.invalid", 6, blob), bytes("in", "b-1@203.0.113.1", 1, other)]
    expect(RfcInherit.ruling([finding({}), peer()], composedBytes).charged).toHaveLength(1)
    const mixed = [message("out", "A-9@pivot.invalid", 6, OFFENDING), bytes("in", "b-1@203.0.113.1", 1, Buffer.from(OFFENDING, "utf8"))]
    expect(RfcInherit.ruling([finding({}), peer()], mixed).charged, "a text line and a bytes line of one body match").toEqual([])
  })

  it("never reads a scripted party's own gating finding as inherited", () => {
    const own = peer({ gating: true })
    const got = RfcInherit.ruling([own], relayed)
    expect(got.inherited).toEqual([])
    expect(got.charged).toEqual([own])
  })

  it("leaves a charge no peer committed standing, whatever the SUT relayed", () => {
    const got = RfcInherit.ruling([finding({})], relayed)
    expect(got.inherited).toEqual([])
    expect(got.charged).toHaveLength(1)
  })

  it("leaves a charge standing where the peer's deviation is a different one", () => {
    const other = peer({
      detail:
        `Answer media type does not match the offer's at 1 stream position(s) — ` +
        `m=[0] offered 'audio', answered 'video' (callId A-9@pivot.invalid, CSeq 6) — RFC 3264 §6.1`
    })
    const got = RfcInherit.ruling([finding({}), other], relayed)
    expect(got.inherited).toEqual([])
    expect(got.charged).toHaveLength(1)
  })

  it("leaves a charge standing on a body the SUT composed rather than carried", () => {
    const composed = [
      message("out", "A-9@pivot.invalid", 6, OFFENDING),
      message("in", "b-1@203.0.113.1", 1, COMPOSED)
    ]
    const got = RfcInherit.ruling([finding({}), peer()], composed)
    expect(got.inherited).toEqual([])
    expect(got.charged).toHaveLength(1)
  })

  it("leaves a charge standing where the SUT sent the bytes on another call", () => {
    const elsewhere = [
      message("out", "A-9@pivot.invalid", 6, OFFENDING),
      message("in", "b-2@203.0.113.1", 1, OFFENDING)
    ]
    expect(RfcInherit.ruling([finding({}), peer()], elsewhere).charged).toHaveLength(1)
  })

  it("leaves a charge standing where the SUT sent the bytes on another CSeq", () => {
    const later = [
      message("out", "A-9@pivot.invalid", 6, OFFENDING),
      message("in", "b-1@203.0.113.1", 4, OFFENDING)
    ]
    expect(RfcInherit.ruling([finding({}), peer()], later).charged).toHaveLength(1)
  })

  it("leaves a charge the rule pinpoints no call for standing", () => {
    const nowhere = finding({ detail: "Answer media type does not match the offer's" })
    const its = peer({ detail: "Answer media type does not match the offer's" })
    expect(RfcInherit.ruling([nowhere, its], relayed).charged).toHaveLength(1)
  })

  it("never reads an empty body as a body the SUT carried", () => {
    const bodiless = [
      { ...message("out", "A-9@pivot.invalid", 6, ""), raw: "SIP/2.0 100 Trying\r\n\r\n" },
      { ...message("in", "b-1@203.0.113.1", 1, ""), raw: "SIP/2.0 100 Trying\r\nCall-ID: b-1@203.0.113.1\r\nCSeq: 1 INVITE\r\n\r\n" }
    ]
    expect(RfcInherit.ruling([finding({}), peer()], bodiless).charged).toHaveLength(1)
  })

  it("rules on gating charges alone: an advisory finding is already recorded", () => {
    const advisory = finding({ advisory: true, gating: false })
    const got = RfcInherit.ruling([advisory, peer()], relayed)
    expect(got.inherited).toEqual([])
    expect(got.charged).toEqual([])
  })

  it("carries the findings the document attributes to a party, apart from the charges", () => {
    const attributed = finding({ gating: false, cancelled_by: "relayed: …", caused_by: "uac1" })
    const got = RfcInherit.ruling([attributed, finding({}), peer()], relayed)
    expect(got.relayed).toEqual([attributed])
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["answer-media-type-matches-offer"])
  })

  it("says nothing at all about a cell no fabric audited", () => {
    expect(RfcInherit.findingsOf(undefined)).toEqual([])
    expect(RfcInherit.findingsOf({ status: "not-audited", reason: "no fabric" })).toEqual([])
  })
})

/** A peer's second description under another session's `o=`: its own break of RFC 3264 §8. */
const SWITCHED = `v=0\r\no=- 9 1 IN IP4 198.51.100.9\r\ns=-\r\nm=audio 4000 RTP/AVP 0\r\n`

/** What the audit wrote of an `o=` that leaves the session, sent by `party` on `callId`. */
const originBreak = (party: string, callId: string, cseq: number, prior: string): string =>
  `${party}: Sent "o=- 9 1 IN IP4 198.51.100.9" after "${prior}" on the same dialog ` +
  `(callId ${callId}, CSeq ${cseq}) — it describes a different session — a later description ` +
  `keeps the o= identity (username, sess-id, nettype, addrtype, address) and moves only ` +
  `sess-version — RFC 4566 §5.2 / RFC 3264 §8`

describe("an sdp-origin-continuity charge on a relayed description", () => {
  const ours = (prior: string) =>
    finding({
      rule: "sdp-origin-continuity",
      detail: originBreak("203.0.113.1:5080", "b-1@203.0.113.1", 2, prior)
    })
  const theirs = peer({
    rule: "sdp-origin-continuity",
    detail: originBreak("203.0.113.1:5060", "A-9@pivot.invalid", 7, "o=- 1 1 IN IP4 198.51.100.1")
  })
  const carried = [message("out", "A-9@pivot.invalid", 7, SWITCHED), message("in", "b-1@203.0.113.1", 2, SWITCHED)]

  it("reads the copy of a peer's own origin break, carried byte for byte, back to the peer", () => {
    const got = RfcInherit.ruling([ours("o=- 1 1 IN IP4 198.51.100.1"), theirs], carried)
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["sdp-origin-continuity"])
    expect(got.charged).toEqual([])
  })

  it("keeps a switch away from a session the SUT itself described charged", () => {
    const got = RfcInherit.ruling([ours("o=- 2 2 IN IP4 198.51.100.2"), theirs], carried)
    expect(got.inherited).toEqual([])
    expect(got.charged).toHaveLength(1)
  })
})

describe("a bodiless response the SUT relays after a peer emitted it", () => {
  const peerFirst = (status: string, cseqIn = "1 INVITE", cseqOut = "6 INVITE") => [
    response("out", 1_423_000, status, "A-9@pivot.invalid", cseqOut, "z9hG4bKf91b"),
    response("in", 1_427_000, status, "b-1@203.0.113.1", cseqIn, "z9hG4bK-B-1")
  ]

  it("reads a 421 without Require back to the peer whose 421 came first", () => {
    const got = RfcInherit.ruling([missing421(), peer421()], peerFirst("421 Extension Required"))
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["unsupported-extension-421"])
    expect(got.charged).toEqual([])
  })

  it("reads a 415 without Accept back to the peer whose 415 came first, across CSeq numbers", () => {
    const acrossCSeqs = [
      response("out", 1_423_000, "415 Unsupported Media Type", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKb78b"),
      response("in", 1_427_000, "415 Unsupported Media Type", "b-1@203.0.113.1", "2 INVITE", "z9hG4bK-B-3")
    ]
    const got = RfcInherit.ruling([missing415(), peer415()], acrossCSeqs)
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["unsupported-415-accepts"])
    expect(got.charged).toEqual([])
  })

  it("reads the same tick as the relay it is: a peer response no later than the SUT's", () => {
    const sameTick = [
      response("out", 1_423_000, "421 Extension Required", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_423_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], sameTick).charged).toEqual([])
  })

  it("leaves a 421 standing where the SUT's precedes the peer's: nothing to inherit from", () => {
    const sutFirst = [
      response("in", 1_423_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1"),
      response("out", 1_427_000, "421 Extension Required", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b")
    ]
    const got = RfcInherit.ruling([missing421(), peer421()], sutFirst)
    expect(got.inherited).toEqual([])
    expect(got.charged).toHaveLength(1)
  })

  it("reads the charged response alone: a relayed 100 Trying pair before a SUT-first 421 proves nothing", () => {
    const trying = [
      response("out", 1_000_000, "100 Trying", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_001_000, "100 Trying", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1"),
      response("in", 1_423_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1"),
      response("out", 1_427_000, "421 Extension Required", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b")
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], trying).charged).toHaveLength(1)
  })

  it("orders first emissions only: the SUT's retransmission after the peer's is no relay", () => {
    const original = response("in", 1_423_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    const retransmitted = [
      original,
      response("out", 1_427_000, "421 Extension Required", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      { ...original, seq: 2, at_us: 1_923_000, repeat_of: 1 }
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], retransmitted).charged).toHaveLength(1)
  })

  it("never reads a peer response on the SUT's own Call-ID as the relayed one", () => {
    const sameCall = [
      response("out", 1_423_000, "421 Extension Required", "b-1@203.0.113.1", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_427_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    const its = peer421({
      detail:
        `203.0.113.1:5080: Sent 421 response (callId b-1@203.0.113.1, branch z9hG4bKf91b) lacks Require header ` +
        `listing required extensions — {Uas} RFC 3261 §21.4.15 / RFC3261-MUST-182`
    })
    expect(RfcInherit.ruling([missing421(), its], sameCall).charged).toHaveLength(1)
  })

  it("pins the SUT's response by its branch: a composed 415 to a re-INVITE is not the relayed initial one", () => {
    const twoTransactions = [
      response("in", 1_000_000, "415 Unsupported Media Type", "b-1@203.0.113.1", "2 INVITE", "z9hG4bK-B-3"),
      response("out", 1_423_000, "415 Unsupported Media Type", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKb78b"),
      response("in", 1_427_000, "415 Unsupported Media Type", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    expect(RfcInherit.ruling([missing415(), peer415()], twoTransactions).charged).toHaveLength(1)
  })

  it("leaves a 421 standing where the peer's response carries another status code", () => {
    const other = [
      response("out", 1_423_000, "420 Bad Extension", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_427_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], other).charged).toHaveLength(1)
  })

  it("leaves a 421 standing where the peer's response answers another method", () => {
    const other = peerFirst("421 Extension Required", "1 INVITE", "6 UPDATE")
    expect(RfcInherit.ruling([missing421(), peer421()], other).charged).toHaveLength(1)
  })

  it("leaves a 421 standing where the earlier peer 421 is not the one the audit charged", () => {
    const elsewhere = [
      response("out", 1_423_000, "421 Extension Required", "A-7@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_427_000, "421 Extension Required", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], elsewhere).charged).toHaveLength(1)
  })

  it("leaves a 421 standing where the SUT's datagram is on another call than the charge names", () => {
    const elsewhere = [
      response("out", 1_423_000, "421 Extension Required", "A-9@pivot.invalid", "6 INVITE", "z9hG4bKf91b"),
      response("in", 1_427_000, "421 Extension Required", "b-2@203.0.113.1", "1 INVITE", "z9hG4bK-B-1")
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], elsewhere).charged).toHaveLength(1)
  })

  it("leaves a 421 standing where no peer finding states the same absence", () => {
    expect(RfcInherit.ruling([missing421()], peerFirst("421 Extension Required")).charged).toHaveLength(1)
  })

  it("never reads a response the SUT put a body of its own on through this arm", () => {
    const [peerSent, sutSent] = peerFirst("421 Extension Required")
    const composed = [
      peerSent,
      {
        ...sutSent,
        raw: Wire.textOf(sutSent).replace("Content-Length: 0\r\n\r\n", `Content-Length: ${COMPOSED.length}\r\n\r\n${COMPOSED}`)
      }
    ]
    expect(RfcInherit.ruling([missing421(), peer421()], composed).charged).toHaveLength(1)
  })
})

describe("a body charge whose rule prints the transaction's branch", () => {
  /** A non-2xx final the audit pins by branch, as `unacked-invite-non-2xx-final` words it. */
  const unacked = (callId: string, branch: string, over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding =>
    finding({
      rule: "unacked-invite-non-2xx-final",
      detail:
        `${partyOf(callId)}: Sent a non-2xx final 488 to an INVITE (callId ${callId}, branch ${branch}) that was never ` +
        `ACKed — the INVITE transaction never completes; RFC 3261 §17.1.1.3`,
      ...over
    })
  const charge = unacked("b-1@203.0.113.1", "z9hG4bK-B-3")
  const its = unacked("A-9@pivot.invalid", "z9hG4bKf91b", {
    lane: "203.0.113.1:5060",
    gating: false,
    charged: "203.0.113.1:5060",
    actor: "ep0"
  })
  const bodied = (dir: Bundle.Dir, callId: string, branch: string, body: string): Bundle.RecordedMessage => ({
    seq: 1,
    dir,
    at_us: 0,
    raw:
      `SIP/2.0 488 Not Acceptable Here\r\nVia: SIP/2.0/UDP 203.0.113.1:5060;branch=${branch}\r\n` +
      `Call-ID: ${callId}\r\nCSeq: 1 INVITE\r\nContent-Type: application/sdp\r\n` +
      `Content-Length: ${body.length}\r\n\r\n${body}`
  })

  it("inherits where the pinned transaction's datagram carries the peer's body", () => {
    const relayedOnBranch = [
      bodied("out", "A-9@pivot.invalid", "z9hG4bKf91b", OFFENDING),
      bodied("in", "b-1@203.0.113.1", "z9hG4bK-B-3", OFFENDING)
    ]
    const got = RfcInherit.ruling([charge, its], relayedOnBranch)
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["unacked-invite-non-2xx-final"])
    expect(got.charged).toEqual([])
  })

  it("stays charged where only another transaction on the same call carries it", () => {
    const otherBranch = [
      bodied("out", "A-9@pivot.invalid", "z9hG4bKf91b", OFFENDING),
      bodied("in", "b-1@203.0.113.1", "z9hG4bK-B-1", OFFENDING),
      bodied("in", "b-1@203.0.113.1", "z9hG4bK-B-3", COMPOSED)
    ]
    expect(RfcInherit.ruling([charge, its], otherBranch).charged).toHaveLength(1)
  })
})

describe("the offending content a detail states", () => {
  it("erases the call identity the two legs necessarily differ in", () => {
    expect(RfcInherit.contentOf("m=[0] (callId b-1@203.0.113.1, CSeq 2) — RFC 3264")).toBe(
      "m=[0] (callId ~, CSeq ~) — RFC 3264"
    )
  })

  it("erases the Via branch with the rest of the transaction identity", () => {
    expect(
      RfcInherit.contentOf("x sent 421 response (callId b-1@203.0.113.1, branch z9hG4bK-A-18d4f0e30e46e91d0-1) lacks Require")
    ).toBe("x sent 421 response (callId ~, branch ~) lacks Require")
  })

  it("erases the party the detail opens on: the same deviation sent by two parties", () => {
    const sut = RfcInherit.contentOf("203.0.113.1:5080: Sent 421 response (callId b-1@h) lacks Require", "203.0.113.1:5080")
    const its = RfcInherit.contentOf("203.0.113.2:0#ep0: Sent 421 response (callId A-9@h) lacks Require", "203.0.113.2:0#ep0")
    expect(sut).toBe("~: Sent 421 response (callId ~) lacks Require")
    expect(its).toBe(sut)
    expect(RfcInherit.contentOf("203.0.113.1:5080: Sent 421", "203.0.113.9:5080")).toBe("203.0.113.1:5080: Sent 421")
  })

  it("erases the party wherever the detail names it, not only where it opens", () => {
    const trying = (party: string) =>
      `${party}: received 2 100 Trying for INVITE CSeq=1 Call-ID=x against 1 INVITE(s) sent — ${party} sent a 100 beyond one per INVITE`
    expect(RfcInherit.contentOf(trying("203.0.113.1:5080"), "203.0.113.1:5080")).toBe(
      "~: received 2 100 Trying for INVITE CSeq=1 Call-ID=x against 1 INVITE(s) sent — ~ sent a 100 beyond one per INVITE"
    )
    expect(RfcInherit.contentOf(trying("203.0.113.1:5080"), "203.0.113.1:5080"))
      .toBe(RfcInherit.contentOf(trying("203.0.113.2:0#ep0"), "203.0.113.2:0#ep0"))
    expect(RfcInherit.contentOf("203.0.113.1:50800: x", "203.0.113.1:5080"), "a longer address is another party").toBe("203.0.113.1:50800: x")
  })

  it("keeps a branch that lacks the cookie: that value is what branch-prefix charges", () => {
    const detail =
      `INVITE top Via branch "abc123" does not begin with the RFC 3261 magic cookie "z9hG4bK" ` +
      `(§8.1.1.7) — a downstream element cannot treat it as an RFC-3261 transaction id`
    expect(RfcInherit.contentOf(detail)).toBe(detail)
  })

  it("keeps every value the rule printed, so two different deviations never compare equal", () => {
    const one = "Sent second reliable 1xx (status 180, RSeq=625709, callId A-9@x) before prior RSeq 625708 PRACKed"
    const two = "Sent second reliable 1xx (status 180, RSeq=5404210, callId b-1@y) before prior RSeq 5404209 PRACKed"
    expect(RfcInherit.contentOf(one)).not.toBe(RfcInherit.contentOf(two))
  })
})

/**
 * The two readings that clear a SUT charge, put to the three relayed violations
 * the document's `relays` statement was made for: an ACK body on a completed
 * round, a 2xx answering no offer, a payload type remapped. The details are the
 * live audit's own wording.
 */
describe("the run's reading beside the document's relays statement", () => {
  const SDP = `v=0\r\no=- 7 7 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 18\r\n`
  const RESTATED = SDP.replace("o=- 7 7", "o=- 3 4")
  const ackBody = (callId: string, cseq: number) =>
    `${partyOf(callId)}: Sent an ACK (callId ${callId}, CSeq ${cseq}) carrying a session description (audio/RTP/AVP) although the ` +
    `offer/answer exchange it acknowledges was already complete — expected no body on this ACK ` +
    `(Content-Length: 0), RFC 3261 §13.2.2.4 / §13.2.1`
  const charge = finding({ rule: "ack-body-after-complete-offer-answer", detail: ackBody("b-1@203.0.113.1", 1) })
  const committed = peer({ rule: "ack-body-after-complete-offer-answer", detail: ackBody("A-9@pivot.invalid", 6), cancelled_by: "stated" })
  const ack = (dir: Bundle.Dir, callId: string, cseq: number, body: string): Bundle.RecordedMessage => ({
    seq: 1,
    dir,
    at_us: 0,
    raw: `ACK sip:b@192.0.2.2 SIP/2.0\r\nCall-ID: ${callId}\r\nCSeq: ${cseq} ACK\r\nContent-Type: application/sdp\r\nContent-Length: ${body.length}\r\n\r\n${body}`
  })

  it("clears a peer's body ours carried byte for byte, the document stating nothing", () => {
    const got = RfcInherit.ruling([charge, committed], [ack("out", "A-9@pivot.invalid", 6, SDP), ack("in", "b-1@203.0.113.1", 1, SDP)])
    expect(got.charged).toEqual([])
    expect(RfcInherit.rulesOf(got.inherited)).toEqual(["ack-body-after-complete-offer-answer"])
  })

  it("leaves standing a peer's body ours re-serialized (a restated origin, a rebooked c=/m=): only the relays statement clears it", () => {
    const got = RfcInherit.ruling([charge, committed], [ack("out", "A-9@pivot.invalid", 6, SDP), ack("in", "b-1@203.0.113.1", 1, RESTATED)])
    expect(got.charged).toEqual([charge])
  })

  it("clears a bodiless 2xx ours relayed after the peer's, the document stating nothing", () => {
    const unanswered = (callId: string, cseq: number) =>
      `${partyOf(callId)}: Sent a 200 (callId ${callId}, CSeq ${cseq}) carrying no session description although the request it ` +
      `answers offered one (audio/RTP/AVP) and no reliable message had answered it — expected the answer in this ` +
      `final, RFC 3261 §13.2.1 / RFC 3264 §6`
    const sut = finding({ rule: "final-2xx-answers-the-offer", detail: unanswered("b-1@203.0.113.1", 1) })
    const its = peer({ rule: "final-2xx-answers-the-offer", detail: unanswered("A-9@pivot.invalid", 6), cancelled_by: "stated" })
    const run = [
      response("out", 10, "200 OK", "A-9@pivot.invalid", "6 INVITE", "z9hG4bK-a"),
      response("in", 12, "200 OK", "b-1@203.0.113.1", "1 INVITE", "z9hG4bK-b")
    ]
    expect(RfcInherit.ruling([sut, its], run).charged).toEqual([])
  })

  it("never re-rules a charge the relays statement cancelled in the run: it arrives non-gating, attributed", () => {
    const cancelled = { ...charge, gating: false, cancelled_by: "relayed: … the reference relays it too", caused_by: "uac1" }
    const got = RfcInherit.ruling([cancelled, committed], [ack("out", "A-9@pivot.invalid", 6, SDP), ack("in", "b-1@203.0.113.1", 1, RESTATED)])
    expect(got).toEqual({ inherited: [], charged: [], relayed: [cancelled], source: [] })
  })
})
