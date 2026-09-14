/**
 * The pipeline's own document-level refusals: a vantage that captured only part
 * of an exchange, and the shapes that only look like one.
 */
import { Tokens, type Flow } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import {
  ACK_NOT_CAPTURED,
  ACTOR_ACK_NOT_CAPTURED,
  FINAL_NOT_CAPTURED,
  orphanLine,
  orphanResponses,
  REQUEST_NOT_CAPTURED,
  unackedFinals,
  unackedLine,
  unackedTakenFinals,
  unackedTakenLine,
  unfinalledAcks,
  unfinalledLine
} from "../src/runnable.js"

const DELAY: Flow.Delay = {
  ms: 0,
  from: Tokens.anchorToken({ _tag: "trigger" }),
  compressible: true,
  timer_linked: false
}

const step = (
  id: string,
  leg: string,
  op: "send" | "expect",
  msg: Flow.Step["msg"],
  auto = false
): Flow.Step => ({ id, leg, op, msg, delay: DELAY, ...(auto ? { auto: true } : {}) })

/**
 * An INVITE carries an offer unless the shape under test is the delayed one: the
 * offer model decides which ACK the §13.3.1.4 give-up can compose, so
 * {@link unackedFinals} reads it and a fixture that left it out would be
 * testing neither arm.
 */
const invite = (id: string, leg: string, op: "send" | "expect" = "send") =>
  step(id, leg, op, { method: "INVITE", body: { ref: "resources/uac_0_0.sdp" } })

/** An INVITE with no offer: the 2xx carries it and the ACK owes the answer. */
const offerless = (id: string, leg: string, op: "send" | "expect" = "send") =>
  step(id, leg, op, { method: "INVITE" })
const final = (id: string, leg: string, status: number, op: "send" | "expect" = "send") =>
  step(id, leg, op, { status, "cseq-method": "INVITE" })
const ack = (id: string, leg: string, op: "send" | "expect" = "send") =>
  step(id, leg, op, { method: "ACK" }, true)

describe("an ACK whose final the leg never captured", () => {
  it("charges the ACK, naming the INVITE its leg had outstanding", () => {
    const charged = unfinalledAcks([
      invite("s1", "A", "expect"),
      step("s2", "A", "send", { status: 180, "cseq-method": "INVITE" }),
      ack("s3", "A", "expect")
    ])
    expect(charged).toEqual([{ ack: "s3", invite: "s1", leg: "A" }])
  })

  it("charges it though the PEER leg captured the final the SUT relays", () => {
    // The answer reaches leg A on the wire, so the ACK still composes — and
    // arrives on a leg holding no step for it. The hole is leg A's either way.
    expect(
      unfinalledAcks([
        invite("s1", "A", "expect"),
        invite("s2", "B", "expect"),
        final("s3", "B", 200),
        ack("s4", "A", "expect")
      ]).map((c) => c.ack)
    ).toEqual(["s4"])
  })

  it("charges a re-INVITE the flow leaves unfinalled though the first was answered", () => {
    const charged = unfinalledAcks([
      invite("s1", "B", "expect"),
      final("s2", "B", 200),
      ack("s3", "B"),
      invite("s4", "B", "expect"),
      ack("s5", "B")
    ])
    expect(charged.map((c) => [c.ack, c.invite])).toEqual([["s5", "s4"]])
  })

  it("counts a non-2xx final: an ACK to a 487 is composed from the 487", () => {
    expect(
      unfinalledAcks([invite("s1", "A", "expect"), final("s2", "A", 487), ack("s3", "A", "expect")])
    ).toEqual([])
  })

  it("leaves a SCRIPTED ACK alone — it states its own coordinates", () => {
    expect(
      unfinalledAcks([invite("s1", "A", "expect"), step("s2", "A", "expect", { method: "ACK" })])
    ).toEqual([])
  })

  it("carries the capture coordinates of both steps into the finding", () => {
    const charged = unfinalledAcks([
      { ...invite("s1", "B", "expect"), observed: { leg: 2, msg: 6, at_us: 10 } },
      { ...ack("s2", "B"), observed: { leg: 2, msg: 8, at_us: 20 } }
    ])
    expect(charged[0]!.observed).toEqual({ leg: 2, msg: 8, at_us: 20 })
    expect(charged[0]!.inviteObserved).toEqual({ leg: 2, msg: 6, at_us: 10 })
    expect(unfinalledLine("capture.pcap.gz", "case1", charged)).toBe(
      `capture.pcap.gz: case 'case1' EXCLUDED ${FINAL_NOT_CAPTURED} — leg B step s2 ` +
        "(capture leg 2 msg 8) ACKs the INVITE at s1 (capture leg 2 msg 6) and the leg " +
        "captured no final answering it"
    )
  })
})

describe("a 2xx whose ACK the leg never captured", () => {
  it("charges the 2xx, naming the INVITE the SUT sent the leg", () => {
    const charged = unackedFinals([
      invite("s1", "A", "expect"),
      step("s2", "A", "send", { status: 100, "cseq-method": "INVITE" }),
      final("s3", "A", 200),
      ack("s4", "B")
    ])
    expect(charged).toEqual([{ final: "s3", invite: "s1", leg: "A" }])
  })

  it("charges an OFFERED dial though no leg captured an ACK — the give-up ACKs it", () => {
    // The caller BYEs instead of ACKing. The hole looks symmetric and is not:
    // the ACK leg B owes needs no answer body, so the §13.3.1.4 give-up puts
    // one there ahead of its BYE whatever leg A does.
    expect(
      unackedFinals([
        invite("s1", "A", "send"),
        final("s2", "A", 200, "expect"),
        invite("s3", "B", "expect"),
        final("s4", "B", 200),
        step("s5", "A", "send", { method: "BYE" })
      ]).map((c) => c.final)
    ).toEqual(["s4"])
  })

  it("stays silent on a DELAYED-OFFER dial no ACK follows — the give-up BYEs it alone", () => {
    // The caller re-INVITEs with no offer and never ACKs the answer, so no ACK
    // this stack can compose exists and the 2xx simply retransmits, exactly as
    // the document states it.
    expect(
      unackedFinals([
        invite("s1", "A", "send"),
        final("s2", "A", 200, "expect"),
        invite("s3", "B", "expect"),
        final("s4", "B", 200),
        ack("s5", "B", "expect"),
        offerless("s6", "A", "send"),
        offerless("s7", "B", "expect"),
        final("s8", "B", 200),
        final("s9", "A", 200, "expect")
      ])
    ).toEqual([])
  })

  it("leaves a 2xx REPEAT the leg's own ACK preceded alone — the trace lost nothing", () => {
    // capture_139737: the answer rode leg B's ACK, the SUT carried it to leg A,
    // leg A ACKed at s6 and re-sent its 200 afterwards. That repeat crossed the
    // ACK in flight (§13.3.1.4); the datagram is on the wire and in the trace.
    expect(
      unackedFinals([
        offerless("s1", "B", "send"),
        offerless("s2", "A", "expect"),
        final("s3", "A", 200),
        final("s4", "B", 200, "expect"),
        ack("s5", "B", "send"),
        ack("s6", "A", "expect"),
        final("s7", "A", 200)
      ])
    ).toEqual([])
  })

  it("charges the leg's NEXT transaction again — an ACK settles one exchange, not the leg", () => {
    expect(
      unackedFinals([
        invite("s1", "A", "expect"),
        final("s2", "A", 200),
        ack("s3", "A", "expect"),
        invite("s4", "A", "expect"),
        final("s5", "A", 200)
      ]).map((c) => c.final)
    ).toEqual(["s5"])
  })

  it("leaves a 2xx the document states as REPEATED alone — the ladder proves the wire", () => {
    // capture_50d3690c: the peer re-sent its 200 and no ACK ever came, which a
    // UAS does only while none has arrived — a source violation, not a hole.
    expect(
      unackedFinals([
        invite("s1", "A", "expect"),
        { ...final("s2", "A", 200), retransmits: 1 }
      ])
    ).toEqual([])
  })

  it("charges a DELAYED-OFFER dial the ingress leg ACKs after it — the SUT relays that one", () => {
    expect(
      unackedFinals([
        offerless("s1", "A", "send"),
        offerless("s2", "B", "expect"),
        final("s3", "B", 200),
        final("s4", "A", 200, "expect"),
        ack("s5", "A", "send")
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("discharges on the ACK the SUT sends back", () => {
    expect(
      unackedFinals([invite("s1", "A", "expect"), final("s2", "A", 200), ack("s3", "A", "expect")])
    ).toEqual([])
  })

  it("charges the terminal re-INVITE though every earlier one was ACKed", () => {
    // capture_92628: two re-INVITEs relayed to the caller, the last one's ACK
    // absent from every vantage.
    const charged = unackedFinals([
      invite("s1", "A", "expect"),
      final("s2", "A", 200),
      ack("s3", "A", "expect"),
      invite("s4", "A", "expect"),
      final("s5", "A", 200),
      ack("s6", "B")
    ])
    expect(charged.map((c) => [c.final, c.invite])).toEqual([["s5", "s4"]])
  })

  it("charges it though a BYE tore the dialog down first — RFC 5407 §2 carves the ACK out", () => {
    expect(
      unackedFinals([
        invite("s1", "A", "expect"),
        final("s2", "A", 200),
        step("s3", "A", "send", { method: "BYE" }),
        step("s4", "A", "expect", { status: 200, "cseq-method": "BYE" }),
        ack("s5", "B")
      ]).map((c) => c.final)
    ).toEqual(["s2"])
  })

  it("leaves the SUT's own 2xx alone — there the ACK is the actor's to send", () => {
    // The actor sent the INVITE, so the ACK is a `send` step the document
    // scripts. Nothing is missing.
    expect(unackedFinals([invite("s1", "A", "send"), step("s2", "A", "expect", { status: 200, "cseq-method": "INVITE" })])).toEqual([])
  })

  it("leaves a non-2xx final alone — its ACK is the client transaction's", () => {
    expect(unackedFinals([invite("s1", "A", "expect"), final("s2", "A", 487)])).toEqual([])
  })

  it("leaves a leg the SUT never answered alone", () => {
    expect(unackedFinals([invite("s1", "A", "expect")])).toEqual([])
  })

  it("carries the capture coordinates of both steps into the finding", () => {
    const charged = unackedFinals([
      { ...invite("s1", "A", "expect"), observed: { leg: 1, msg: 9, at_us: 10 } },
      { ...final("s2", "A", 200), observed: { leg: 1, msg: 11, at_us: 20 } },
      ack("s3", "B")
    ])
    expect(charged[0]!.observed).toEqual({ leg: 1, msg: 11, at_us: 20 })
    expect(charged[0]!.inviteObserved).toEqual({ leg: 1, msg: 9, at_us: 10 })
    expect(unackedLine("capture.pcap.gz", "case1", charged)).toBe(
      `capture.pcap.gz: case 'case1' EXCLUDED ${ACK_NOT_CAPTURED} — leg A step s2 ` +
        "(capture leg 1 msg 11) answers the SUT's INVITE at s1 (capture leg 1 msg 9) and " +
        "the leg captured no ACK for it"
    )
  })
})

describe("a response whose request the leg never captured", () => {
  const options = (id: string, leg: string, op: "send" | "expect") =>
    step(id, leg, op, { method: "OPTIONS" })
  const okTo = (id: string, leg: string, op: "send" | "expect", m: string) =>
    step(id, leg, op, { status: 200, "cseq-method": m })

  it("charges an answer the leg was never given a request for", () => {
    // The capture's own keepalive cadence, minus the two request datagrams the
    // vantage dropped: the answers survive and nothing opens their transaction.
    expect(
      orphanResponses([
        options("s1", "B", "expect"),
        okTo("s2", "B", "send", "OPTIONS"),
        okTo("s3", "B", "send", "OPTIONS")
      ]).map((c) => c.response)
    ).toEqual(["s3"])
  })

  it("charges an answer awaited for a request the leg never sends", () => {
    expect(
      orphanResponses([invite("s1", "A", "send"), okTo("s2", "B", "expect", "BYE")]).map(
        (c) => c.response
      )
    ).toEqual(["s2"])
  })

  it("matches a response against the requests travelling the other way", () => {
    // Leg B expects the INVITE and answers it; leg A sends one and awaits the
    // answer. Neither direction may satisfy the other.
    expect(
      orphanResponses([
        invite("s1", "A", "send"),
        invite("s2", "B", "expect"),
        okTo("s3", "B", "send", "INVITE"),
        okTo("s4", "A", "expect", "INVITE")
      ])
    ).toEqual([])
  })

  it("keeps the INVITE transaction open until the ACK, so a 2xx may repeat", () => {
    // RFC 3261 §13.3.1.4: the 2xx retransmits until the ACK arrives.
    expect(
      orphanResponses([
        invite("s1", "A", "expect"),
        final("s2", "A", 200),
        final("s3", "A", 200),
        ack("s4", "A", "expect"),
        step("s5", "A", "send", { status: 200, "cseq-method": "INVITE" })
      ]).map((c) => c.response)
    ).toEqual(["s5"])
  })

  it("lets a CANCEL open its own transaction", () => {
    expect(
      orphanResponses([
        invite("s1", "B", "expect"),
        step("s2", "B", "expect", { method: "CANCEL" }),
        okTo("s3", "B", "send", "CANCEL"),
        final("s4", "B", 487)
      ])
    ).toEqual([])
  })

  it("is silent on a document whose vantage captured every request", () => {
    expect(
      orphanResponses([
        invite("s1", "A", "send"),
        step("s2", "A", "expect", { status: 100, "cseq-method": "INVITE" }, true),
        invite("s3", "B", "expect"),
        final("s4", "B", 200),
        okTo("s5", "A", "expect", "INVITE"),
        ack("s6", "A", "send"),
        ack("s7", "B", "expect")
      ])
    ).toEqual([])
  })

  it("names the response, its leg and the transaction it answers", () => {
    const charged = orphanResponses([okTo("s1", "B", "send", "OPTIONS")])
    expect(charged).toEqual([{ response: "s1", leg: "B", status: 200, method: "OPTIONS" }])
    expect(orphanLine("capture_1.pcap.gz", "case1", charged)).toBe(
      `capture_1.pcap.gz: case 'case1' EXCLUDED ${REQUEST_NOT_CAPTURED} — ` +
        "leg B step s1 carries a 200 to OPTIONS and the leg captured no OPTIONS it answers"
    )
  })
})

describe("a dialog-creating 2xx the ACTOR took and never ACKed", () => {
  /** A re-INVITE, the in-dialog transaction that proves the ACK crossed. */
  const reinvite = (id: string, leg: string, op: "send" | "expect" = "send") =>
    offerless(id, leg, op)

  it("charges the 2xx, naming the INVITE the actor sent and the transaction that follows", () => {
    const charged = unackedTakenFinals([
      offerless("s1", "A"),
      final("s2", "A", 200, "expect"),
      reinvite("s3", "A"),
      final("s4", "A", 200, "expect"),
      ack("s5", "A")
    ])
    expect(charged).toEqual([
      { final: "s2", invite: "s1", leg: "A", ground: "continuation", continuation: "s3", method: "INVITE" }
    ])
  })

  it("charges an OFFERED dial too — the peer leg's ACK is this one, relayed", () => {
    // The actor offers, never ACKs the answer, and re-INVITEs anyway. The peer
    // leg's ACK step waits on the ACK this leg withheld, offer or none.
    expect(
      unackedTakenFinals([
        invite("s1", "A"),
        final("s2", "A", 200, "expect"),
        reinvite("s3", "A"),
        final("s4", "A", 200, "expect"),
        ack("s5", "A")
      ]).map((c) => [c.final, c.invite, c.continuation])
    ).toEqual([["s2", "s1", "s3"]])
  })

  it("keeps a 2xx whose ACK lands AFTER a premature re-INVITE the peer answered 491", () => {
    // The actor re-INVITEs over its own un-ACKed 2xx (§14.1), the peer answers
    // 491 (RFC 6026 Accepted), the ACK to the 491 discharges the re-INVITE, and
    // the ACK to the original 2xx follows: the leg captured it, nothing was lost.
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        reinvite("s3", "A"),
        final("s4", "A", 491, "expect"),
        ack("s5", "A"),
        ack("s6", "A"),
        step("s7", "A", "send", { method: "BYE" })
      ])
    ).toEqual([])
  })

  it("keeps it when the 491 round's ACK is the only one the leg carries — nothing proves a loss", () => {
    // The 491's ACK is the re-INVITE's (§17.1.1.3), so the 2xx stands unsettled;
    // but the 491 is the peer saying the dialog is still unconfirmed, and the BYE
    // is the actor giving up — the never-ACKing corner the rule replays.
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        reinvite("s3", "A"),
        final("s4", "A", 491, "expect"),
        ack("s5", "A"),
        step("s6", "A", "send", { method: "BYE" })
      ])
    ).toEqual([])
  })

  it("charges it on the INFO past a 491'd re-INVITE — the next continuation still counts", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        reinvite("s3", "A"),
        final("s4", "A", 491, "expect"),
        ack("s5", "A"),
        step("s6", "A", "send", { method: "INFO" })
      ]).map((c) => [c.final, c.invite, c.continuation])
    ).toEqual([["s2", "s1", "s6"]])
  })

  it("charges a second 2xx to an INVITE already ACKed once — each 2xx received draws its own ACK", () => {
    // A fork's other-To-tag 2xx, or a re-emission past the transaction's
    // envelope, is its own step and its own ACK owed (RFC 3261 §13.2.2.4).
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        ack("s3", "A"),
        final("s4", "A", 200, "expect"),
        step("s5", "A", "send", { method: "INFO" })
      ]).map((c) => [c.final, c.invite, c.continuation])
    ).toEqual([["s4", "s1", "s5"]])
  })

  it("takes an in-dialog request the SUT sends INTO the leg as the same proof", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        step("s3", "A", "expect", { method: "UPDATE" })
      ]).map((c) => [c.continuation, c.method])
    ).toEqual([["s3", "UPDATE"]])
  })

  it("keeps the abandoned dialog: nothing follows the 2xx, so nothing says the ACK was sent", () => {
    expect(unackedTakenFinals([offerless("s1", "A"), final("s2", "A", 200, "expect")])).toEqual([])
  })

  it("keeps it though the platform reaps: a BYE is what an un-ACKed 2xx draws", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        step("s3", "A", "expect", { method: "BYE" }),
        step("s4", "A", "send", { status: 200, "cseq-method": "BYE" })
      ])
    ).toEqual([])
  })

  it("leaves an ACKed 2xx alone", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        ack("s3", "A"),
        reinvite("s4", "A")
      ])
    ).toEqual([])
  })

  it("leaves the SUT-owed direction to unackedFinals", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "B", "expect"),
        final("s2", "B", 200),
        reinvite("s3", "B", "expect")
      ])
    ).toEqual([])
  })

  it("counts a non-2xx final out: its ACK composes from the final itself", () => {
    expect(
      unackedTakenFinals([offerless("s1", "A"), final("s2", "A", 486, "expect"), reinvite("s3", "A")])
    ).toEqual([])
  })

  it("charges a re-INVITE the actor left unACKed though the first was ACKed", () => {
    const charged = unackedTakenFinals([
      offerless("s1", "A"),
      final("s2", "A", 200, "expect"),
      ack("s3", "A"),
      reinvite("s4", "A"),
      final("s5", "A", 200, "expect"),
      step("s6", "A", "send", { method: "INFO" })
    ])
    expect(charged.map((c) => [c.final, c.invite, c.continuation])).toEqual([["s5", "s4", "s6"]])
  })

  /** A step at a capture instant, in ms from the capture's origin. */
  const at = (s: Flow.Step, ms: number, msg = 0): Flow.Step => ({
    ...s,
    observed: { leg: s.leg === "A" ? 1 : 2, msg, at_us: ms * 1000 }
  })

  /**
   * The actor dials, the far party answers, the SUT relays the 2xx and the far
   * leg expects the ACK the SUT relays back; the actor's leg then carries only a
   * BYE. Both proofs of a lost ACK sit in the 2xx itself: `retransmits` (a
   * repeated 2xx is one no ACK reached, §13.3.1.4) and `observed.at_us` (the
   * silence the leg measured after it, which is proof only past the first rung).
   */
  const relayedShape = (twoxx: Partial<Flow.Step>, byeAtMs: number): ReadonlyArray<Flow.Step> => [
    at(offerless("s1", "A"), 0),
    at(offerless("s2", "B", "expect"), 50),
    at(final("s3", "B", 200), 1000, 5),
    at({ ...final("s4", "A", 200, "expect"), ...twoxx }, 1010, 3),
    at(ack("s5", "B", "expect"), 1056, 6),
    at(step("s6", "A", "send", { method: "BYE" }), byeAtMs, 4)
  ]

  it("charges a 2xx the platform never repeated when the far leg expects its ACK relayed", () => {
    // No rung behind the 2xx over 25 s and the far leg's ACK step 46 ms after
    // it: the actor's ACK reached the platform and the trace lost it.
    const charged = unackedTakenFinals(relayedShape({}, 26_010))
    expect(charged).toEqual([
      {
        final: "s4",
        invite: "s1",
        leg: "A",
        ground: "relayed-ack",
        continuation: "s5",
        method: "ACK",
        groundLeg: "B",
        silenceMs: 25_000,
        observed: { leg: 1, msg: 3, at_us: 1_010_000 },
        inviteObserved: { leg: 1, msg: 0, at_us: 0 },
        continuationObserved: { leg: 2, msg: 6, at_us: 1_056_000 }
      }
    ])
    expect(unackedTakenLine("capture.pcap.gz", "auto", charged)).toBe(
      `capture.pcap.gz: case 'auto' EXCLUDED ${ACTOR_ACK_NOT_CAPTURED} — leg A step s4 ` +
        "(capture leg 1 msg 3) answers the actor's INVITE at s1 (capture leg 1 msg 0) and " +
        "the leg captured no ACK for it, yet the 2xx never repeated over the 25000 ms the leg " +
        "stayed silent and leg B step s5 (capture leg 2 msg 6) expects that ACK relayed"
    )
  })

  it("keeps a 2xx the document declares REPEATED — the ladder is the proof no ACK reached it", () => {
    expect(unackedTakenFinals(relayedShape({ retransmits: 1 }, 26_010))).toEqual([])
  })

  it("keeps a 2xx the leg is silent behind for less than the first rung — nothing was measured", () => {
    expect(unackedTakenFinals(relayedShape({}, 1_400))).toEqual([])
    expect(unackedTakenFinals(relayedShape({}, 1_510)).map((c) => c.silenceMs)).toEqual([500])
  })

  it("carries the capture coordinates of all three steps into the finding", () => {
    const charged = unackedTakenFinals([
      { ...offerless("s1", "A"), observed: { leg: 1, msg: 0, at_us: 0 } },
      { ...final("s2", "A", 200, "expect"), observed: { leg: 1, msg: 3, at_us: 30 } },
      { ...reinvite("s3", "A"), observed: { leg: 1, msg: 4, at_us: 40 } }
    ])
    expect(unackedTakenLine("capture.pcap.gz", "case1", charged)).toBe(
      `capture.pcap.gz: case 'case1' EXCLUDED ${ACTOR_ACK_NOT_CAPTURED} — leg A step s2 ` +
        "(capture leg 1 msg 3) answers the actor's INVITE at s1 (capture leg 1 msg 0) and " +
        "the leg captured no ACK for it, yet goes on to carry the INVITE at s3 " +
        "(capture leg 1 msg 4)"
    )
  })
})
