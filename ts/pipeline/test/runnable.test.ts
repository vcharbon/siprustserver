/**
 * The pipeline's own document-level refusals: a vantage that captured only part
 * of an exchange, and the shapes that only look like one.
 */
import { Schedules, Tokens, type Flow } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import {
  ACK_NOT_CAPTURED,
  ACTOR_ACK_NOT_CAPTURED,
  answererEndsDialogFirst,
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
    // A captured shape: the answer rode leg B's ACK, the SUT carried it to leg A,
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
    // A captured shape: the peer re-sent its 200 and no ACK ever came, which a
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
    // A captured shape: two re-INVITEs relayed to the caller, the last one's ACK
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

/** The step placed at `ms` on the capture's clock. */
const at = (s: Flow.Step, ms: number): Flow.Step => ({
  ...s,
  observed: { leg: 0, msg: 0, at_us: ms * 1000 }
})
const bye = (id: string, leg: string, op: "send" | "expect") => step(id, leg, op, { method: "BYE" })
const byeOk = (id: string, leg: string, op: "send" | "expect") =>
  step(id, leg, op, { status: 200, "cseq-method": "BYE" })

/** 64·T1, the bound of the 2xx ladder (RFC 3261 §13.3.1.4). */
const GIVE_UP_MS = Schedules.SCHEDULES.classes.find((c) => c.class === "final-2xx")!.give_up_ms

const cancel = (id: string, leg: string, op: "send" | "expect") =>
  step(id, leg, op, { method: "CANCEL" })

/**
 * A dial leg A places through the SUT to leg B, answered and relayed: the 2xx
 * at s3 is the one the SUT waits on leg A's ACK to acknowledge.
 */
const relayedAnswer = [
  at(invite("s1", "A", "send"), 0),
  at(invite("s2", "B", "expect"), 100),
  at(final("s3", "B", 200), 1_000),
  at(final("s4", "A", 200, "expect"), 1_100)
]

describe("a 2xx whose answerer ends the dialog before any ACK reaches it", () => {
  it("leaves the callee's 2xx alone where the callee BYEs first and nobody ever ACKs", () => {
    // The caller never ACKs and the callee hangs up well inside the give-up: the
    // dialog is gone before the SUT composes an ACK, and none exists to relay.
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(bye("s5", "B", "send"), 10_000),
        at(byeOk("s6", "B", "expect"), 10_100),
        at(bye("s7", "A", "expect"), 10_200),
        at(byeOk("s8", "A", "send"), 10_300)
      ])
    ).toEqual([])
  })

  it("leaves a re-INVITE's 2xx alone where its answerer BYEs before any ACK", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(final("s3", "B", 200), 1_000),
        at(final("s4", "A", 200, "expect"), 1_100),
        at(ack("s5", "A", "send"), 1_200),
        at(ack("s6", "B", "expect"), 1_300),
        at(invite("s7", "B", "send"), 2_000),
        at(invite("s8", "A", "expect"), 2_100),
        at(final("s9", "A", 200), 2_200),
        at(final("s10", "B", 200, "expect"), 2_300),
        at(bye("s11", "A", "send"), 2_400),
        at(byeOk("s12", "A", "expect"), 2_500),
        at(bye("s13", "B", "expect"), 2_600),
        at(byeOk("s14", "B", "send"), 2_700)
      ])
    ).toEqual([])
  })

  it("leaves it alone where the answerer's BYE lands just inside the give-up", () => {
    expect(
      unackedFinals([...relayedAnswer, at(bye("s5", "B", "send"), 1_000 + GIVE_UP_MS - 1)])
    ).toEqual([])
  })

  it("charges it where the answerer's BYE comes at the give-up — the give-up ACKed first", () => {
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(bye("s5", "B", "send"), 1_000 + GIVE_UP_MS),
        at(byeOk("s6", "B", "expect"), 1_100 + GIVE_UP_MS)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where the far party ACKs after the BYE — RFC 5407 §2 relays that ACK", () => {
    expect(
      unackedFinals([
        at(invite("s1", "B", "send"), 0),
        at(invite("s2", "A", "expect"), 100),
        at(final("s3", "A", 200), 1_000),
        at(final("s4", "B", 200, "expect"), 1_100),
        at(bye("s5", "A", "send"), 2_000),
        at(byeOk("s6", "A", "expect"), 2_100),
        at(ack("s7", "B"), 3_000)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where the other party's BYE comes first — the SUT's teardown ACKs ahead of its BYE", () => {
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(bye("s5", "A", "send"), 2_000),
        at(bye("s6", "B", "send"), 2_100),
        at(byeOk("s7", "B", "expect"), 2_200)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where the SUT BYEs the leg first — its teardown ACKs ahead of that BYE", () => {
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(bye("s5", "B", "expect"), 2_000),
        at(bye("s6", "B", "send"), 2_100)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where the SUT BYEs another leg first — its teardown ACKs this one too", () => {
    // The teardown crosses the answerer's own BYE: the SUT ACKs leg B ahead of
    // the BYE it sends there, whichever leg its release started on.
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(bye("s5", "A", "expect"), 2_000),
        at(bye("s6", "B", "send"), 2_002),
        at(bye("s7", "B", "expect"), 2_003)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where the capture states no instant — nothing places the BYE inside the give-up", () => {
    expect(
      unackedFinals([
        invite("s1", "A", "send"),
        invite("s2", "B", "expect"),
        final("s3", "B", 200),
        final("s4", "A", 200, "expect"),
        bye("s5", "B", "send")
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges it where only one of the two instants is stated", () => {
    const [i1, i2, , f4] = relayedAnswer
    expect(
      unackedFinals([i1!, i2!, final("s3", "B", 200), f4!, at(bye("s5", "B", "send"), 2_000)])
        .map((c) => c.final)
    ).toEqual(["s3"])
    expect(
      unackedFinals([...relayedAnswer, bye("s5", "B", "send")]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges a 2xx to the SUT's own dial — no ingress ACK is awaited, the SUT ACKs on arrival", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "expect"), 0),
        at(final("s2", "A", 200), 100),
        at(bye("s3", "A", "send"), 200),
        at(byeOk("s4", "A", "expect"), 300)
      ]).map((c) => c.final)
    ).toEqual(["s2"])
  })

  it("charges a 2xx to the SUT's own re-INVITE — it relays that 2xx to no one", () => {
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(ack("s5", "A", "send"), 1_200),
        at(ack("s6", "B", "expect"), 1_300),
        at(invite("s7", "B", "expect"), 2_000),
        at(final("s8", "B", 200), 2_100),
        at(bye("s9", "B", "send"), 2_200),
        at(byeOk("s10", "B", "expect"), 2_300),
        at(bye("s11", "A", "expect"), 2_400),
        at(byeOk("s12", "A", "send"), 2_500)
      ]).map((c) => c.final)
    ).toEqual(["s8"])
  })

  it("charges a 2xx on a leg the SUT dials once the ingress dial is settled", () => {
    // A new leg C the SUT dials after leg A's dial completed: its 2xx answers no
    // INVITE an ingress party still has pending, so the SUT ACKs it itself.
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(ack("s5", "A", "send"), 1_200),
        at(ack("s6", "B", "expect"), 1_300),
        at(invite("s7", "C", "expect"), 2_000),
        at(final("s8", "C", 200), 2_500),
        at(invite("s9", "A", "expect"), 2_510),
        at(final("s10", "A", 200), 2_600),
        at(ack("s11", "A", "expect"), 2_610),
        at(bye("s12", "C", "send"), 5_000)
      ]).map((c) => c.final)
    ).toEqual(["s8"])
  })

  it("charges a 2xx crossing the CANCEL the SUT relayed — the SUT ACKs and BYEs it (§9.1)", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(cancel("s3", "A", "send"), 300),
        at(cancel("s4", "B", "expect"), 310),
        at(final("s5", "B", 200), 320),
        at(final("s6", "A", 487, "expect"), 330),
        at(bye("s7", "B", "send"), 5_000)
      ]).map((c) => c.final)
    ).toEqual(["s5"])
  })

  it("charges a 2xx the CANCEL crossed though the SUT relayed that 2xx onward", () => {
    // The relayed 2xx alone does not settle it: the CANCEL the leg took ahead of
    // its 2xx is what makes the SUT ACK that 2xx itself.
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(cancel("s3", "A", "send"), 300),
        at(cancel("s4", "B", "expect"), 310),
        at(final("s5", "B", 200), 320),
        at(final("s6", "A", 200, "expect"), 330),
        at(bye("s7", "B", "send"), 5_000)
      ]).map((c) => c.final)
    ).toEqual(["s5"])
  })

  /** A 2xx answering the forked INVITE on the early dialog `tag`. */
  const forked = (id: string, tag: string): Flow.Step => ({ ...final(id, "B", 200), early: tag })

  it("charges a forked 2xx landing behind the first one — the SUT ACKs and BYEs that fork itself", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(forked("s3", "e1"), 1_000),
        at(forked("s4", "e2"), 1_010),
        at(final("s5", "A", 200, "expect"), 1_100),
        at(bye("s6", "B", "send"), 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s4"])
  })

  it("charges a forked 2xx landing behind the first one though its own fork BYEs", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(forked("s3", "e1"), 1_000),
        at(forked("s4", "e2"), 1_010),
        at(final("s5", "A", 200, "expect"), 1_100),
        at({ ...bye("s6", "B", "send"), early: "e2" }, 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s4"])
  })

  it("charges a forked 2xx landing after the relayed one", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(forked("s3", "e1"), 1_000),
        at(final("s4", "A", 200, "expect"), 1_100),
        at(forked("s5", "e2"), 1_200),
        at(bye("s6", "B", "send"), 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s5"])
  })

  it("charges the relayed 2xx where the BYE rides another fork's dialog", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(forked("s3", "e1"), 1_000),
        at(final("s4", "A", 200, "expect"), 1_100),
        at({ ...bye("s5", "B", "send"), early: "e2" }, 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s3"])
  })

  it("charges a 2xx whose relay the document places ahead of it — no relay follows it", () => {
    expect(
      unackedFinals([
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(final("s3", "A", 200, "expect"), 1_000),
        at(final("s4", "B", 200), 1_010),
        at(bye("s5", "B", "send"), 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s4"])
  })

  it("charges a 2xx relayed to an ingress INVITE opened after the SUT's own", () => {
    // Leg A's re-INVITE pending when the SUT re-INVITEs B is finalised and
    // replaced; the 2xx A expects answers the replacement, so the SUT's 2xx on
    // B answers no dial still waiting on it. A's ACK to the 487 is off the capture.
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(ack("s5", "A", "send"), 1_200),
        at(ack("s6", "B", "expect"), 1_300),
        at(invite("s7", "A", "send"), 2_000),
        at(invite("s8", "B", "expect"), 2_100),
        at(cancel("s9", "A", "send"), 2_200),
        at(final("s10", "A", 487, "expect"), 2_300),
        at(invite("s11", "A", "send"), 2_400),
        at(final("s12", "B", 200), 2_500),
        at(final("s13", "A", 200, "expect"), 2_600),
        at(bye("s14", "B", "send"), 6_000)
      ]).map((c) => c.final)
    ).toEqual(["s12"])
  })

  it("charges a 2xx on a leg the SUT dialled before the ingress re-INVITE it answers", () => {
    // The SUT dials C with nothing pending on leg A; A's later re-INVITE is
    // answered by the SUT, and C's 2xx is the SUT's own to ACK.
    expect(
      unackedFinals([
        ...relayedAnswer,
        at(ack("s5", "A", "send"), 1_200),
        at(ack("s6", "B", "expect"), 1_300),
        at(invite("s7", "C", "expect"), 2_000),
        at(final("s8", "C", 200), 2_100),
        at(invite("s9", "A", "send"), 2_200),
        at(final("s10", "A", 200, "expect"), 2_300),
        at(bye("s11", "C", "send"), 5_000)
      ]).map((c) => c.final)
    ).toEqual(["s8"])
  })
})

describe("the answerer's own BYE ahead of any ACK, as one predicate", () => {
  it("names the answerer's BYE and how long after its 2xx it came", () => {
    expect(
      answererEndsDialogFirst([...relayedAnswer, at(bye("s5", "B", "send"), 10_000)], 2)
    ).toEqual({ step: "s5", afterUs: 9_000_000 })
  })

  /** A BYE in the confirmed dialog carries no early-dialog stamp; it rides the first 2xx's dialog. */
  it("names an unstamped BYE that follows a forked 2xx", () => {
    const flow = [
      at(invite("s1", "A", "send"), 0),
      at(invite("s2", "B", "expect"), 100),
      at({ ...final("s3", "B", 200), early: "e1" }, 1_000),
      at(final("s4", "A", 200, "expect"), 1_100),
      at(bye("s5", "B", "send"), 10_000)
    ]
    expect(answererEndsDialogFirst(flow, 2)).toEqual({ step: "s5", afterUs: 9_000_000 })
    expect(unackedFinals(flow)).toEqual([])
  })

  it("answers nothing where the BYE comes at the give-up", () => {
    expect(
      answererEndsDialogFirst([...relayedAnswer, at(bye("s5", "B", "send"), 1_000 + GIVE_UP_MS)], 2)
    ).toBeUndefined()
  })

  it("answers nothing at a step that is no 2xx the answerer sends", () => {
    const flow = [...relayedAnswer, at(bye("s5", "B", "send"), 10_000)]
    expect(answererEndsDialogFirst(flow, 1)).toBeUndefined()
    expect(answererEndsDialogFirst(flow, 3)).toBeUndefined()
  })

  /** Each shape puts an ACK of the SUT's on the answerer's leg ahead of that BYE. */
  const ackedAhead: ReadonlyArray<readonly [string, ReadonlyArray<Flow.Step>, number]> = [
    [
      "a CANCEL the leg took before the 2xx",
      [
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(cancel("s3", "A", "send"), 300),
        at(cancel("s4", "B", "expect"), 310),
        at(final("s5", "B", 200), 320),
        at(final("s6", "A", 200, "expect"), 330),
        at(bye("s7", "B", "send"), 5_000)
      ],
      4
    ],
    [
      "a 2xx to the SUT's own dial",
      [at(invite("s1", "A", "expect"), 0), at(final("s2", "A", 200), 100), at(bye("s3", "A", "send"), 200)],
      1
    ],
    [
      "a BYE on another fork's early dialog",
      [
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at({ ...final("s3", "B", 200), early: "e1" }, 1_000),
        at(final("s4", "A", 200, "expect"), 1_100),
        at({ ...bye("s5", "B", "send"), early: "e2" }, 6_000)
      ],
      2
    ],
    [
      "an ACK on another leg between the INVITE and the 2xx",
      [
        at(invite("s1", "A", "send"), 0),
        at(invite("s2", "B", "expect"), 100),
        at(ack("s3", "A", "send"), 500),
        at(final("s4", "B", 200), 1_000),
        at(final("s5", "A", 200, "expect"), 1_100),
        at(bye("s6", "B", "send"), 2_000)
      ],
      3
    ],
    [
      "a far party's ACK after the BYE",
      [...relayedAnswer, at(bye("s5", "B", "send"), 2_000), at(ack("s6", "A", "send"), 3_000)],
      2
    ],
    [
      "the SUT's BYE on another leg first",
      [...relayedAnswer, at(bye("s5", "A", "expect"), 2_000), at(bye("s6", "B", "send"), 2_002)],
      2
    ]
  ]

  it.each(ackedAhead)("answers nothing for %s", (_shape, flow, at2xx) => {
    expect(answererEndsDialogFirst(flow, at2xx)).toBeUndefined()
  })
})

describe("a 2xx to an INVITE a release caught open", () => {
  /** A call answered and ACKed end to end, leg A dialling leg B through the SUT. */
  const confirmed = [
    invite("s1", "A", "send"),
    invite("s2", "B", "expect"),
    final("s3", "B", 200),
    final("s4", "A", 200, "expect"),
    ack("s5", "A", "send"),
    ack("s6", "B", "expect")
  ]

  it("leaves the 2xx alone where the SUT's BYE reaches the leg while the re-INVITE is open", () => {
    // The caller re-INVITEs and hangs up before the callee answers; the SUT ends
    // the open re-INVITE with its release, so the 2xx scripted after it is moot.
    expect(
      unackedFinals([
        ...confirmed,
        invite("s7", "A", "send"),
        invite("s8", "B", "expect"),
        step("s9", "B", "send", { status: 100, "cseq-method": "INVITE" }, true),
        bye("s10", "A", "send"),
        byeOk("s11", "A", "expect"),
        bye("s12", "B", "expect"),
        final("s13", "B", 200),
        byeOk("s14", "B", "send")
      ])
    ).toEqual([])
  })

  it("charges a re-INVITE's 2xx the leg answered before the release", () => {
    expect(
      unackedFinals([
        ...confirmed,
        invite("s7", "A", "send"),
        invite("s8", "B", "expect"),
        final("s9", "B", 200),
        bye("s10", "A", "send"),
        byeOk("s11", "A", "expect"),
        bye("s12", "B", "expect"),
        byeOk("s13", "B", "send")
      ]).map((c) => c.final)
    ).toEqual(["s9"])
  })

  it("charges a 2xx repeated past the release of an INVITE answered before it", () => {
    // The re-INVITE's first 2xx closed the transaction ahead of the BYE, so the
    // release caught nothing open and the repeat past it is still owed its ACK.
    expect(
      unackedFinals([
        ...confirmed,
        invite("s7", "A", "send"),
        invite("s8", "B", "expect"),
        final("s9", "B", 200),
        final("s10", "A", 200, "expect"),
        bye("s11", "B", "expect"),
        final("s12", "B", 200)
      ]).map((c) => c.final)
    ).toEqual(["s12"])
  })

  it("charges the 2xx where the answerer's own BYE crossed its re-INVITE — the SUT ACKs it", () => {
    // The answerer BYEs with the re-INVITE open and then answers it: the SUT
    // CANCELs that re-INVITE on the BYE, the 2xx crosses the CANCEL, and §9.1
    // has the SUT ACK the 2xx it receives.
    expect(
      unackedFinals([
        ...confirmed,
        invite("s7", "A", "send"),
        invite("s8", "B", "expect"),
        bye("s9", "B", "send"),
        final("s10", "B", 200),
        byeOk("s11", "B", "expect")
      ]).map((c) => c.final)
    ).toEqual(["s10"])
  })

  it("charges a later re-INVITE's 2xx — the leg's next INVITE clears the release mark", () => {
    // The SUT's BYE draws a 481 and the call goes on: the next re-INVITE opens a
    // transaction no release has caught.
    expect(
      unackedFinals([
        ...confirmed,
        invite("s7", "A", "send"),
        invite("s8", "B", "expect"),
        bye("s9", "B", "expect"),
        step("s10", "B", "send", { status: 481, "cseq-method": "BYE" }),
        final("s11", "B", 200),
        final("s12", "A", 200, "expect"),
        ack("s13", "A", "send"),
        ack("s14", "B", "expect"),
        invite("s15", "A", "send"),
        invite("s16", "B", "expect"),
        final("s17", "B", 200),
        final("s18", "A", 200, "expect")
      ]).map((c) => c.final)
    ).toEqual(["s17"])
  })

  it("charges the 2xx to an initial INVITE the SUT BYEd in its early dialog", () => {
    expect(
      unackedFinals([
        invite("s1", "A", "send"),
        invite("s2", "B", "expect"),
        final("s3", "B", 180),
        bye("s4", "B", "expect"),
        final("s5", "B", 200),
        byeOk("s6", "B", "send")
      ]).map((c) => c.final)
    ).toEqual(["s5"])
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
      { final: "s2", invite: "s1", leg: "A", ground: "continuation", proof: "s3", method: "INVITE" }
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
      ]).map((c) => [c.final, c.invite, c.proof])
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
      ]).map((c) => [c.final, c.invite, c.proof])
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
      ]).map((c) => [c.final, c.invite, c.proof])
    ).toEqual([["s4", "s1", "s5"]])
  })

  it("takes an in-dialog request the SUT sends INTO the leg as the same proof", () => {
    expect(
      unackedTakenFinals([
        offerless("s1", "A"),
        final("s2", "A", 200, "expect"),
        step("s3", "A", "expect", { method: "UPDATE" })
      ]).map((c) => [c.proof, c.ground === "continuation" ? c.method : undefined])
    ).toEqual([["s3", "UPDATE"]])
  })

  it("keeps the abandoned dialog: nothing follows the 2xx, so nothing says the ACK was sent", () => {
    expect(unackedTakenFinals([offerless("s1", "A"), final("s2", "A", 200, "expect")])).toEqual([])
  })

  it("keeps a re-INVITE the actor sends on a leg it answered, ACKed by its own step", () => {
    // The answering side re-INVITEs: the 2xx it takes is settled by the ACK it
    // sends, and the 2xx it SENT is not this rule's (the peer owes that ACK).
    expect(
      unackedTakenFinals([
        offerless("s1", "B", "expect"),
        final("s2", "B", 200),
        reinvite("s3", "B"),
        final("s4", "B", 200, "expect"),
        ack("s5", "B")
      ])
    ).toEqual([])
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
    expect(charged.map((c) => [c.final, c.invite, c.proof])).toEqual([["s5", "s4", "s6"]])
  })

  const LEG_NO: Record<string, number> = { A: 1, B: 2, C: 3 }

  /** A step at a capture instant, in ms from the capture's origin. */
  const at = (s: Flow.Step, ms: number, msg = 0): Flow.Step => ({
    ...s,
    observed: { leg: LEG_NO[s.leg]!, msg, at_us: ms * 1000 }
  })

  /** A step whose delay the cut anchored on another step, as the classifier stamps it. */
  const anchored = (s: Flow.Step, on: string): Flow.Step => ({
    ...s,
    delay: { ...s.delay, from: Tokens.anchorToken({ _tag: "step", step: on }) }
  })

  const bye = (id: string, leg: string, op: "send" | "expect" = "send") =>
    step(id, leg, op, { method: "BYE" })

  /**
   * The actor dials, the far party answers, the SUT relays the 2xx (the
   * actor's 2xx anchored on the far leg's send, as the cut stamps a relay) and
   * the far leg expects the ACK the SUT relays back; the actor's leg then
   * carries only a BYE. Both gates of the proof sit in the 2xx itself:
   * `retransmits` / `retransmit_intervals_ms` (where the ladder stopped, and
   * the rung the far leg's ACK must follow) and `observed.at_us` (the silence
   * the leg measured after it, read only past the rung that was due).
   */
  const relayedShape = (twoxx: Partial<Flow.Step>, byeAtMs: number): ReadonlyArray<Flow.Step> => [
    at(offerless("s1", "A"), 0),
    at(offerless("s2", "B", "expect"), 50),
    at(final("s3", "B", 200), 1000, 5),
    at(anchored({ ...final("s4", "A", 200, "expect"), ...twoxx }, "s3"), 1010, 3),
    at(ack("s5", "B", "expect"), 1056, 6),
    at(bye("s6", "A"), byeAtMs, 4)
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
        proof: "s5",
        groundLeg: "B",
        silenceMs: 25_000,
        observed: { leg: 1, msg: 3, at_us: 1_010_000 },
        inviteObserved: { leg: 1, msg: 0, at_us: 0 },
        proofObserved: { leg: 2, msg: 6, at_us: 1_056_000 }
      }
    ])
    expect(unackedTakenLine("capture.pcap.gz", "auto", charged)).toBe(
      `capture.pcap.gz: case 'auto' EXCLUDED ${ACTOR_ACK_NOT_CAPTURED} — leg A step s4 ` +
        "(capture leg 1 msg 3) answers the actor's INVITE at s1 (capture leg 1 msg 0) and " +
        "the leg captured no ACK for it, yet the 2xx never repeated over the 25000 ms the leg " +
        "stayed silent and leg B step s5 (capture leg 2 msg 6) expects that ACK relayed"
    )
  })

  it("keeps a 2xx repeated AFTER the far leg's ACK step — a rung past the ACK is no ladder that stopped", () => {
    // One rung at T1 (1510) and the far leg's ACK at 1056, before it: the ladder
    // fired past the ACK, so its end proves nothing about when the ACK reached.
    expect(unackedTakenFinals(relayedShape({ retransmits: 1 }, 26_010))).toEqual([])
    // At the rung's own instant is not after it.
    expect(unackedTakenFinals(laddered([500], 1_510, 26_010))).toEqual([])
  })

  /** The schedule's own rungs, T1 first, to the give-up. */
  const SCHEDULE = Schedules.rungIntervalsMs("final-2xx")

  /**
   * {@link relayedShape} with the actor's 2xx declaring a ladder of the given
   * gaps and the far leg's ACK step at `ackAtMs`: the ladder ran `rungs` rungs
   * from the 2xx at 1010 (the last at 1010 + their sum) and stopped.
   */
  const laddered = (
    rungs: ReadonlyArray<number>,
    ackAtMs: number,
    byeAtMs: number,
    twoxx: Partial<Flow.Step> = {}
  ): ReadonlyArray<Flow.Step> => [
    ...relayedShape({ retransmits: rungs.length, retransmit_intervals_ms: rungs, ...twoxx }, 0).slice(0, 4),
    at(ack("s5", "B", "expect"), ackAtMs, 6),
    at(bye("s6", "A"), byeAtMs, 4)
  ]

  it("charges a 2xx whose ladder STOPPED short of its schedule with the far leg's ACK after the last rung", () => {
    // One rung at T1, then 25 s of silence where §13.3.1.4 would have run the
    // ladder on to its give-up; the far leg's ACK step 60 ms past that rung.
    const charged = unackedTakenFinals(laddered([500], 1_570, 26_010))
    expect(charged).toEqual([
      {
        final: "s4",
        invite: "s1",
        leg: "A",
        ground: "relayed-ack",
        proof: "s5",
        groundLeg: "B",
        silenceMs: 25_000,
        ladder: { rungs: 1, lastRungMs: 500, dueMs: 1_500 },
        observed: { leg: 1, msg: 3, at_us: 1_010_000 },
        inviteObserved: { leg: 1, msg: 0, at_us: 0 },
        proofObserved: { leg: 2, msg: 6, at_us: 1_570_000 }
      }
    ])
    expect(unackedTakenLine("capture.pcap.gz", "auto", charged)).toBe(
      `capture.pcap.gz: case 'auto' EXCLUDED ${ACTOR_ACK_NOT_CAPTURED} — leg A step s4 ` +
        "(capture leg 1 msg 3) answers the actor's INVITE at s1 (capture leg 1 msg 0) and " +
        "the leg captured no ACK for it, yet the 2xx's ladder stopped after 1 rung at +500 ms " +
        "where the next was due at +1500 ms, over the 25000 ms the leg stayed silent, and leg B " +
        "step s5 (capture leg 2 msg 6) expects that ACK relayed after that rung"
    )
    // The far leg's ACK a millisecond past the rung is after it.
    expect(unackedTakenFinals(laddered([500], 1_511, 26_010)).map((c) => c.proof)).toEqual(["s5"])
  })

  it("keeps a stopped ladder the leg went quiet behind before the next rung was due", () => {
    // One rung at T1; the next was due at T1 + 2·T1 = 1500 ms after the 2xx.
    expect(unackedTakenFinals(laddered([500], 1_570, 1_910))).toEqual([])
    // The boundary: a silence ending AT the due instant gave it no instant to fire.
    expect(unackedTakenFinals(laddered([500], 1_570, 2_510))).toEqual([])
    expect(unackedTakenFinals(laddered([500], 1_570, 2_511)).map((c) => c.ground === "relayed-ack" && c.silenceMs))
      .toEqual([1_501])
    // Two rungs at their measured pace: the third was due 2000 ms after the second.
    expect(unackedTakenFinals(laddered([499, 1_000], 2_600, 4_509))).toEqual([])
    expect(unackedTakenFinals(laddered([499, 1_000], 2_600, 4_510)).map((c) => c.ground === "relayed-ack" && c.ladder))
      .toEqual([{ rungs: 2, lastRungMs: 1_499, dueMs: 3_499 }])
  })

  it("keeps a ladder run to the schedule's end — the platform's own statement that no ACK came", () => {
    const sum = SCHEDULE.reduce((a, g) => a + g, 0)
    expect(unackedTakenFinals(laddered(SCHEDULE, 1_010 + sum + 60, 90_000))).toEqual([])
    // A count past the schedule is a ladder the RFC has already given up on.
    expect(unackedTakenFinals(laddered([...SCHEDULE, 4_000], 1_010 + sum + 4_060, 90_000))).toEqual([])
    // One rung short of the end is a ladder that stopped: the last rung was
    // still due, and the far leg's ACK follows the ninth.
    const short = SCHEDULE.slice(0, -1)
    const shortSum = short.reduce((a, g) => a + g, 0)
    expect(
      unackedTakenFinals(laddered(short, 1_010 + shortSum + 60, 90_000))
        .map((c) => c.ground === "relayed-ack" && c.ladder)
    ).toEqual([{ rungs: 9, lastRungMs: shortSum, dueMs: sum }])
  })

  it("paces a ladder the document states no gaps for on the schedule", () => {
    // Two rungs, no measured intervals: the schedule puts them at 500 and 1500
    // ms and the third due at 3500 ms.
    const unpaced = (ackAtMs: number, byeAtMs: number): ReadonlyArray<Flow.Step> => [
      ...relayedShape({ retransmits: 2 }, 0).slice(0, 4),
      at(ack("s5", "B", "expect"), ackAtMs, 6),
      at(bye("s6", "A"), byeAtMs, 4)
    ]
    expect(unackedTakenFinals(unpaced(2_560, 4_510))).toEqual([])
    expect(unackedTakenFinals(unpaced(2_510, 26_010))).toEqual([])
    expect(unackedTakenFinals(unpaced(2_560, 4_511)).map((c) => c.ground === "relayed-ack" && c.ladder))
      .toEqual([{ rungs: 2, lastRungMs: 1_500, dueMs: 3_500 }])
  })

  it("keeps a stopped ladder whose far-leg ACK states no coordinate to set against the rung", () => {
    expect(
      unackedTakenFinals(
        laddered([500], 1_570, 26_010).map((s) => {
          if (s.id !== "s5") return s
          const { observed: _, ...rest } = s
          return rest
        })
      )
    ).toEqual([])
  })

  it("keeps a 2xx the leg is silent behind for the first rung or less — the ladder had no instant to fire", () => {
    expect(unackedTakenFinals(relayedShape({}, 1_400))).toEqual([])
    expect(unackedTakenFinals(relayedShape({}, 1_510))).toEqual([])
    expect(unackedTakenFinals(relayedShape({}, 1_511)).map((c) => c.ground === "relayed-ack" && c.silenceMs))
      .toEqual([501])
    // A half-millisecond past the rung is still within it: the window rounds down.
    expect(
      unackedTakenFinals([
        ...relayedShape({}, 0).slice(0, 5),
        { ...bye("s6", "A"), observed: { leg: 1, msg: 4, at_us: 1_510_500 } }
      ])
    ).toEqual([])
  })

  it("keeps a 2xx no far leg answered — the actor's own answers on the leg are not a far leg", () => {
    // The SUT dialled the actor, the actor answered and was ACKed; its later
    // re-INVITE the SUT answered itself, and no other leg holds an ACK for it.
    expect(
      unackedTakenFinals([
        at(offerless("s1", "A", "expect"), 0),
        at(final("s2", "A", 200), 10),
        at(ack("s3", "A", "expect"), 20),
        at(offerless("s4", "A"), 500),
        at(final("s5", "A", 200, "expect"), 1_000),
        at(bye("s6", "A"), 26_000)
      ])
    ).toEqual([])
  })

  it("measures the silence to the step that ends the dialog, past a 491'd re-INVITE", () => {
    // The ladder is folded over the whole envelope, so a step that keeps the
    // dialog going does not close the window; the BYE 25 s later does.
    expect(
      unackedTakenFinals([
        ...relayedShape({}, 26_000).slice(0, 5),
        at(offerless("s6", "A"), 1_410),
        at(final("s7", "A", 491, "expect"), 1_420),
        at(ack("s8", "A"), 1_425),
        at(bye("s9", "A"), 26_000)
      ]).map((c) => [c.final, c.proof, c.ground === "relayed-ack" && c.silenceMs])
    ).toEqual([["s4", "s5", 24_990]])
  })

  it("keeps a 2xx whose silence no capture coordinate bounds", () => {
    const shape = relayedShape({}, 26_010)
    const unstated = (s: Flow.Step): Flow.Step => {
      const { observed: _, ...rest } = s
      return rest
    }
    expect(unackedTakenFinals(shape.map((s) => (s.id === "s4" ? unstated(s) : s)))).toEqual([])
    expect(unackedTakenFinals(shape.map((s) => (s.id === "s6" ? unstated(s) : s)))).toEqual([])
  })

  it("names the far leg by the anchor the cut stamped, not by the nearest answer", () => {
    // A fork: B answers 503 and expects the SUT's ACK to it, C answers 200 and
    // the SUT relays that one. The ACK the proof rests on is C's.
    expect(
      unackedTakenFinals([
        at(offerless("s1", "A"), 0),
        at(offerless("s2", "B", "expect"), 50),
        at(offerless("s3", "C", "expect"), 60),
        at(final("s4", "B", 503), 900),
        at(ack("s5", "B", "expect"), 910),
        at(final("s6", "C", 200), 1_000),
        at(anchored(final("s7", "A", 200, "expect"), "s6"), 1_010),
        at(ack("s8", "C", "expect"), 1_056),
        at(bye("s9", "A"), 26_000)
      ]).map((c) => [c.proof, c.ground === "relayed-ack" && c.groundLeg])
    ).toEqual([["s8", "C"]])
  })

  it("keeps it where the far leg opens a new INVITE before its ACK — that ACK is the new one's", () => {
    expect(
      unackedTakenFinals([
        ...relayedShape({}, 26_010).slice(0, 4),
        at(offerless("s7", "B", "expect"), 1_040),
        at(ack("s5", "B", "expect"), 1_056),
        at(bye("s6", "A"), 26_010)
      ])
    ).toEqual([])
  })

  it("finds the far leg's 2xx within relay proximity when the vantage stamped it AFTER the actor's", () => {
    // Two captures, two clocks: the relayed 2xx sits 30 ms before the far leg's
    // send in the merged order, and no anchor is stated.
    expect(
      unackedTakenFinals([
        at(offerless("s1", "A"), 0),
        at(offerless("s2", "B", "expect"), 50),
        at(final("s3", "A", 200, "expect"), 1_000),
        at(final("s4", "B", 200), 1_030),
        at(ack("s5", "B", "expect"), 1_076),
        at(bye("s6", "A"), 26_000)
      ]).map((c) => [c.final, c.proof])
    ).toEqual([["s3", "s5"]])
  })

  it("finds the far leg's 2xx by proximity where the cut anchored a skewed relay on the actor's own leg", () => {
    // The classifier reads only what precedes an arrival, so a relay the far
    // vantage stamped later is anchored on the leg's own 100: the anchor names
    // no far leg, the instant does.
    expect(
      unackedTakenFinals([
        at(offerless("s1", "A"), 0),
        at(offerless("s2", "B", "expect"), 50),
        at(step("s3", "A", "expect", { status: 100, "cseq-method": "INVITE" }, true), 60),
        at(anchored(final("s4", "A", 200, "expect"), "s3"), 1_000),
        at(final("s5", "B", 200), 1_030),
        at(ack("s6", "B", "expect"), 1_076),
        at(bye("s7", "A"), 26_000)
      ]).map((c) => [c.final, c.proof])
    ).toEqual([["s4", "s6"]])
  })

  it("keeps a 2xx no far leg answered within relay proximity — the SUT minted it", () => {
    // The actor's re-INVITE the SUT answered itself, seconds after the far
    // leg's last answer: no relay, so no far-leg ACK is the actor's.
    expect(
      unackedTakenFinals([
        ...relayedShape({}, 26_010).slice(0, 5),
        at(ack("s6", "A"), 1_020),
        at(offerless("s7", "A"), 900_005),
        at(anchored(final("s8", "A", 200, "expect"), "s7"), 900_020),
        at(bye("s9", "A"), 930_000)
      ])
    ).toEqual([])
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
