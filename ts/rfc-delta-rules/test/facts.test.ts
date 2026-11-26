/**
 * The facts the generic rows read, each off a document (and a run where it
 * reads one): what it names, and the neighbouring ladder it leaves out.
 */
import { documentReadingOf } from "@sip/delta-rules/document-reading"
import type { Classifier } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  acknowledgedRSeqAtStep,
  byeBesideCancelSteps,
  cancelCrossingAnswerSteps,
  cancelPastAnswerSteps,
  dialledAfterCancelSteps,
  referOutcomeAtStep,
  releasedLegSteps,
  rfcFactsOf,
  roundsPastReleaseSteps,
  tryingOnEarlyByeSteps,
  tryingOnNonInviteSteps,
  untriedInviteSteps
} from "../src/facts.js"
import { NO_FACTS } from "./fixtures.js"

const step = (id: string, leg: string, op: "send" | "expect", over: Partial<Classifier.DocumentStep>): Classifier.DocumentStep =>
  ({ id, leg, op, auto: false, optional: false, inDialog: false, ...over })

const documentOf = (
  steps: ReadonlyArray<Classifier.DocumentStep>,
  attempts: ReadonlyArray<Classifier.DocumentAttempt> = []
): Classifier.DocumentShape => ({ unackedFinals: [], calls: 1, steps, attempts })

const read = (steps: ReadonlyArray<Classifier.DocumentStep>, attempts: ReadonlyArray<Classifier.DocumentAttempt> = []) =>
  documentReadingOf(documentOf(steps, attempts))

const runOf = (over: Partial<Classifier.RunShape> = {}): Classifier.RunShape => ({
  messages: 10,
  legs: 2,
  legsTornDownBySystem: 0,
  sentRSeqs: new Map(),
  openers: new Map(),
  receptions: new Map(),
  sent: new Map(),
  byeAheadOfInviteFinal: new Set(),
  ...over
})

const runSending = (rseqs: ReadonlyArray<readonly [string, ReadonlyArray<number>]>): Classifier.RunShape =>
  runOf({ sentRSeqs: new Map(rseqs) })

const receiving = (received: Record<string, ReadonlyArray<Classifier.Reception>>): Classifier.RunShape =>
  runOf({ receptions: new Map(Object.entries(received)) })

/** The caller's INVITE ringing on one early dialog of the callee. */
const ringing = [
  step("s1", "A", "send", { method: "INVITE" }),
  step("s3", "B", "expect", { method: "INVITE" }),
  step("s5", "B", "send", { status: 183, cseqMethod: "INVITE" }),
  step("s6", "A", "expect", { status: 183, cseqMethod: "INVITE" })
]

/** The caller's INVITE answered 2xx by the callee. */
const answered = [
  step("s1", "A", "send", { method: "INVITE" }),
  step("s3", "B", "expect", { method: "INVITE" }),
  step("s5", "B", "send", { status: 200, cseqMethod: "INVITE" }),
  step("s6", "A", "expect", { status: 200, cseqMethod: "INVITE" })
]

describe("the RSeq a PRACK acknowledges (RFC 3262 §7.2)", () => {
  it("pairs each of the system's PRACKs with the actor's RSeq of the reliable provisional before it, by position", () => {
    const steps = [
      step("s5", "B", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s7", "B", "expect", { method: "PRACK" }),
      step("s8", "B", "send", { status: 180, cseqMethod: "INVITE" }),
      step("s9", "B", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s10", "B", "expect", { method: "PRACK" }),
      step("s11", "A", "expect", { method: "PRACK" })
    ]
    expect([...acknowledgedRSeqAtStep(read(steps), runSending([["B", [4711, 4712]]]))]).toEqual([["s7", 4711], ["s10", 4712]])
  })

  it("counts a reliable provisional, not the INVITE that asked for reliability", () => {
    const steps = [
      step("s1", "A", "send", { method: "INVITE", reliable: true, sdp: true }),
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    expect([...acknowledgedRSeqAtStep(read(steps), runSending([["A", [7, 8]]]))]).toEqual([["s3", 7]])
  })

  it("leaves out a PRACK whose provisional the run never sent", () => {
    const steps = [
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    expect([...acknowledgedRSeqAtStep(read(steps), runSending([]))]).toEqual([])
  })
})

describe("every fact, taken whole from rfcFactsOf", () => {
  it("states each fact the rows read, the one a deployment's case facts spread", () => {
    const steps = [
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    const facts = rfcFactsOf(read(steps), runSending([["A", [7]]]))
    expect(Object.keys(facts).sort()).toEqual(Object.keys(NO_FACTS).sort())
    expect([...facts.acknowledgedRSeqAtStep]).toEqual([["s3", 7]])
  })
})

describe("a leg's release and the rounds past it (RFC 3261 §15.1, §12.2.2)", () => {
  const legA = [
    step("s19", "A", "send", { method: "NOTIFY", inDialog: true }),
    step("s23", "A", "expect", { status: 200, cseqMethod: "NOTIFY", inDialog: true }),
    step("s25", "A", "expect", { method: "BYE", inDialog: true }),
    step("s26", "A", "send", { status: 200, cseqMethod: "BYE", inDialog: true }),
    step("s27", "A", "send", { method: "NOTIFY", inDialog: true }),
    step("s28", "A", "expect", { status: 501, cseqMethod: "NOTIFY", inDialog: true })
  ]
  const caughtOpen = [
    step("s17", "A", "send", { method: "NOTIFY", inDialog: true }),
    step("s18", "A", "send", { method: "BYE", inDialog: true }),
    step("s20", "A", "expect", { status: 200, cseqMethod: "NOTIFY", inDialog: true }),
    step("s21", "A", "expect", { status: 200, cseqMethod: "BYE", inDialog: true })
  ]

  it("names every step after the release, not the release itself, and nothing on a leg with no BYE", () => {
    expect([...releasedLegSteps(read(legA))]).toEqual(["s26", "s27", "s28"])
    expect([...releasedLegSteps(read(caughtOpen))]).toEqual(["s20", "s21"])
    expect([...releasedLegSteps(read(caughtOpen.filter((s) => s.id !== "s18")))]).toEqual([])
  })

  it("names the answer to a request sent after the release, never a transaction the release caught open", () => {
    expect([...roundsPastReleaseSteps(read(legA))]).toEqual(["s28"])
    expect([...roundsPastReleaseSteps(read(caughtOpen))]).toEqual([])
  })

  it("leaves out the BYE family: a second BYE past the release is the teardown glare's", () => {
    const twice = [
      step("s10", "B", "expect", { method: "BYE", inDialog: true }),
      step("s11", "B", "send", { status: 200, cseqMethod: "BYE", inDialog: true }),
      step("s12", "B", "send", { method: "BYE", inDialog: true }),
      step("s13", "B", "expect", { status: 481, cseqMethod: "BYE", inDialog: true })
    ]
    expect([...roundsPastReleaseSteps(read(twice))]).toEqual([])
  })
})

describe("a 100 to a BYE or CANCEL (RFC 3261 §9.2, §15.1.2, §12.2.2)", () => {
  const byeAfterCancel = [
    ...ringing,
    step("s11", "A", "send", { method: "CANCEL" }),
    step("s12", "A", "send", { method: "BYE" }),
    step("s13", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
    step("s14", "A", "expect", { status: 487, cseqMethod: "INVITE" }),
    step("s22", "A", "expect", { status: 100, cseqMethod: "BYE", auto: true }),
    step("s23", "A", "expect", { status: 481, cseqMethod: "BYE" })
  ]

  it("maps each 100 to the final the dialog state owes the request", () => {
    expect([...tryingOnNonInviteSteps(read(byeAfterCancel))]).toEqual([["s22", 481]])
    const byeOnPending = [...ringing, step("s12", "A", "send", { method: "BYE" }), step("s22", "A", "expect", { status: 100, cseqMethod: "BYE", auto: true })]
    expect([...tryingOnNonInviteSteps(read(byeOnPending))]).toEqual([["s22", 200]])
    const cancelAnswered = [...ringing, step("s11", "A", "send", { method: "CANCEL" }), step("s12", "A", "expect", { status: 100, cseqMethod: "CANCEL", auto: true })]
    expect([...tryingOnNonInviteSteps(read(cancelAnswered))]).toEqual([["s12", 200]])
  })

  /** The run where the caller's BYE left before the INVITE's final reached her. */
  const byeFirst = runOf({ byeAheadOfInviteFinal: new Set(["A"]) })

  it("names the BYE sent after the CANCEL and before any INVITE final as one a 200 answers too", () => {
    expect([...tryingOnEarlyByeSteps(read(byeAfterCancel), byeFirst)]).toEqual(["s22"])
    const byeAfterFinal = [
      ...ringing,
      step("s11", "A", "send", { method: "CANCEL" }),
      step("s13", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
      step("s14", "A", "expect", { status: 487, cseqMethod: "INVITE" }),
      step("s15", "A", "send", { method: "BYE" }),
      step("s22", "A", "expect", { status: 100, cseqMethod: "BYE", auto: true })
    ]
    expect([...tryingOnNonInviteSteps(read(byeAfterFinal))]).toEqual([["s22", 481]])
    expect(tryingOnEarlyByeSteps(read(byeAfterFinal), byeFirst).size).toBe(0)
  })

  it("names no such BYE where the run's final reached the caller before her BYE left", () => {
    expect(tryingOnEarlyByeSteps(read(byeAfterCancel), runOf()).size).toBe(0)
  })

  it("names no step whose scripted final differs from the owed one, nor a 100 to the INVITE", () => {
    const disagreeing = byeAfterCancel.map((s) => (s.id === "s23" ? { ...s, status: 200 } : s))
    expect(tryingOnNonInviteSteps(read(disagreeing)).size).toBe(0)
    expect(tryingOnNonInviteSteps(read([...ringing, step("s2", "A", "expect", { status: 100, cseqMethod: "INVITE", auto: true })])).size).toBe(0)
  })
})

describe("a caller's CANCEL and BYE on an early dialog (RFC 3261 §9.1)", () => {
  const tail = [
    step("s15", "B", "expect", { method: "CANCEL" }),
    step("s16", "B", "expect", { method: "BYE", inDialog: true }),
    step("s18", "B", "send", { status: 200, cseqMethod: "CANCEL" }),
    step("s19", "B", "send", { status: 200, cseqMethod: "BYE" }),
    step("s20", "B", "send", { status: 487, cseqMethod: "INVITE" })
  ]

  it("names the callee's BYE where the caller sent both, in either order", () => {
    const cancelThenBye = [...ringing, step("s11", "A", "send", { method: "CANCEL" }), step("s12", "A", "send", { method: "BYE", inDialog: true }), ...tail]
    const byeThenCancel = [...ringing, step("s11", "A", "send", { method: "BYE", inDialog: true }), step("s12", "A", "send", { method: "CANCEL" }), ...tail]
    expect([...byeBesideCancelSteps(read(cancelThenBye))]).toEqual(["s16"])
    expect([...byeBesideCancelSteps(read(byeThenCancel))]).toEqual(["s16"])
  })

  it("names nothing on a dialog the callee answered, or where the caller only BYEs", () => {
    const confirmed = [
      ...ringing,
      step("s9", "B", "send", { status: 200, cseqMethod: "INVITE" }),
      step("s11", "A", "send", { method: "CANCEL" }),
      step("s12", "A", "send", { method: "BYE", inDialog: true }),
      ...tail
    ]
    const byeOnly = [...ringing, step("s12", "A", "send", { method: "BYE", inDialog: true }), tail[1]!]
    expect(byeBesideCancelSteps(read(confirmed)).size).toBe(0)
    expect(byeBesideCancelSteps(read(byeOnly)).size).toBe(0)
  })
})

describe("a CANCEL past the INVITE's 2xx (RFC 3261 §9.1, §9.2)", () => {
  const cancelPastAnswer = [
    step("s1", "A", "send", { method: "INVITE" }),
    step("s3", "B", "expect", { method: "INVITE" }),
    step("s9", "B", "send", { status: 200, cseqMethod: "INVITE" }),
    step("s10", "A", "expect", { status: 200, cseqMethod: "INVITE" }),
    step("s11", "A", "send", { method: "CANCEL" }),
    step("s12", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
    step("s13", "B", "expect", { method: "CANCEL" }),
    step("s14", "A", "send", { method: "ACK", inDialog: true }),
    step("s16", "B", "expect", { method: "ACK", inDialog: true })
  ]
  const answeredRun = receiving({ A: [{ status: 200, cseqMethod: "INVITE" }, { status: 200, cseqMethod: "CANCEL" }] })

  it("names the caller's 200 to it where a 200 reached her, and the CANCEL toward the callee that answered", () => {
    expect([...cancelPastAnswerSteps(read(cancelPastAnswer), answeredRun)].sort()).toEqual(["s12", "s13"])
    expect([...cancelPastAnswerSteps(read(cancelPastAnswer), runOf())]).toEqual(["s13"])
  })

  it("names nothing for a CANCEL that left before the 2xx", () => {
    const cancelBeforeAnswer = [
      step("s1", "A", "send", { method: "INVITE" }),
      step("s3", "B", "expect", { method: "INVITE" }),
      step("s11", "A", "send", { method: "CANCEL" }),
      step("s12", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
      step("s13", "B", "expect", { method: "CANCEL" }),
      step("s9", "B", "send", { status: 200, cseqMethod: "INVITE" }),
      step("s10", "A", "expect", { status: 200, cseqMethod: "INVITE" })
    ]
    expect(cancelPastAnswerSteps(read(cancelBeforeAnswer), answeredRun).size).toBe(0)
  })
})

describe("a callee's 2xx that left before the caller's CANCEL (RFC 3261 §9.2)", () => {
  const callAB: ReadonlyArray<Classifier.DocumentAttempt> = [{ call: "c1", callerLeg: "A", leg: "B" }]
  const crossing = [
    ...ringing,
    step("s11", "B", "send", { status: 200, cseqMethod: "INVITE" }),
    step("s12", "A", "send", { method: "CANCEL" }),
    step("s13", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
    step("s14", "A", "expect", { status: 487, cseqMethod: "INVITE" }),
    step("s15", "B", "expect", { method: "CANCEL" })
  ]

  it("names the caller's 487 where her own call's callee answered first", () => {
    expect([...cancelCrossingAnswerSteps(read(crossing, callAB))]).toEqual(["s14"])
  })

  it("names nothing where the CANCEL won, or where no attempt ties the answering leg to her call", () => {
    const cancelWon = [
      ...ringing,
      step("s12", "A", "send", { method: "CANCEL" }),
      step("s13", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
      step("s15", "B", "expect", { method: "CANCEL" }),
      step("s16", "B", "send", { status: 200, cseqMethod: "INVITE" }),
      step("s14", "A", "expect", { status: 487, cseqMethod: "INVITE" })
    ]
    expect(cancelCrossingAnswerSteps(read(cancelWon, callAB)).size).toBe(0)
    expect(cancelCrossingAnswerSteps(read(crossing)).size).toBe(0)
  })
})

describe("a REFER's outcome reported after a progress NOTIFY (RFC 3515 §2.4.5)", () => {
  const oneNotify = [
    ...answered,
    step("s9", "B", "send", { method: "REFER", inDialog: true }),
    step("s10", "B", "expect", { status: 202, cseqMethod: "REFER", inDialog: true }),
    step("s11", "B", "expect", { method: "NOTIFY", inDialog: true }),
    step("s12", "B", "send", { status: 200, cseqMethod: "NOTIFY", inDialog: true })
  ]
  const progress: Classifier.Reception = { step: "s11", method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "pending", fragStatus: 100 }
  const terminal: Classifier.Reception = { method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "terminated", fragStatus: 403 }

  it("maps the one NOTIFY a document scripts for a REFER to the outcome the run's terminal NOTIFY reports", () => {
    expect([...referOutcomeAtStep(read(oneNotify), receiving({ B: [progress, terminal] }))]).toEqual([["s11", 403]])
  })

  it("names nothing where the document scripts two NOTIFYs, or the run shows no terminal one", () => {
    const twoNotifies = [
      ...oneNotify,
      step("s13", "B", "expect", { method: "NOTIFY", inDialog: true }),
      step("s14", "B", "send", { status: 200, cseqMethod: "NOTIFY", inDialog: true })
    ]
    expect(referOutcomeAtStep(read(twoNotifies), receiving({ B: [progress, terminal] })).size).toBe(0)
    expect(referOutcomeAtStep(read(oneNotify), receiving({ B: [progress] })).size).toBe(0)
    expect(referOutcomeAtStep(read(oneNotify), receiving({ B: [progress, { ...terminal, subscriptionState: "active" }] })).size).toBe(0)
  })
})

describe("the system's provisional to an INVITE the document scripts no 100 for (RFC 3261 §17.2.1)", () => {
  const reinvites = [
    step("s11", "B", "send", { method: "INVITE", inDialog: true }),
    step("s14", "B", "expect", { status: 200, cseqMethod: "INVITE", inDialog: true }),
    step("s17", "B", "send", { method: "INVITE", inDialog: true }),
    step("s18", "B", "expect", { status: 100, cseqMethod: "INVITE", auto: true, inDialog: true }),
    step("s22", "B", "expect", { status: 200, cseqMethod: "INVITE", inDialog: true })
  ]

  it("names the response of the transaction the document scripts no 100 for, and only that one", () => {
    expect([...untriedInviteSteps(read(reinvites))]).toEqual(["s14"])
  })

  it("names nothing on an INVITE the system sent", () => {
    const sent = [
      step("s3", "B", "expect", { method: "INVITE" }),
      step("s5", "B", "send", { status: 180, cseqMethod: "INVITE" })
    ]
    expect(untriedInviteSteps(read(sent)).size).toBe(0)
  })
})

describe("a call placed after the caller's CANCEL (RFC 3261 §9.2)", () => {
  const cancelledFirst = [
    step("s1", "A", "send", { method: "INVITE" }),
    step("s3", "A", "send", { method: "CANCEL" }),
    step("s4", "B", "expect", { method: "INVITE" }),
    step("s5", "A", "expect", { status: 200, cseqMethod: "CANCEL" }),
    step("s6", "A", "expect", { status: 487, cseqMethod: "INVITE" }),
    step("s8", "B", "expect", { method: "CANCEL" })
  ]
  const callAB: ReadonlyArray<Classifier.DocumentAttempt> = [{ call: "c1", callerLeg: "A", leg: "B", status: 487 }]

  it("names the opening INVITE of her own call's leg dialled after her CANCEL and released after", () => {
    expect([...dialledAfterCancelSteps(read(cancelledFirst, callAB))]).toEqual(["s4"])
  })

  it("names nothing where the dial left first, the leg was never released, or no attempt ties the leg to her call", () => {
    const dialledFirst = [cancelledFirst[0]!, cancelledFirst[2]!, cancelledFirst[1]!, ...cancelledFirst.slice(3)]
    expect(dialledAfterCancelSteps(read(dialledFirst, callAB)).size).toBe(0)
    expect(dialledAfterCancelSteps(read(cancelledFirst.filter((s) => s.id !== "s8"), callAB)).size).toBe(0)
    expect(dialledAfterCancelSteps(read(cancelledFirst)).size).toBe(0)
  })
})
