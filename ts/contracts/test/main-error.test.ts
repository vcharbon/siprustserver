/**
 * The main error of one cell, on its own: which of a cell's several sayings
 * the queue leads with, and how the two "died waiting" failures are spelled.
 *
 * The precedence is the whole claim: a charge the audit stands behind beats
 * the step the run died at, which beats any other failure, and a cell that
 * passed or never ran says nothing at all.
 */
import type * as Bundle from "../src/bundle/index.js"
import type * as Confrontation from "../src/confrontation.js"
import { describe, expect, it } from "vitest"
import { citation, failingBody, gatedOnText, mainErrorOf } from "../src/main-error.js"

const finding = (
  rule: string,
  detail: string,
  offending?: number
): Bundle.RfcFinding => ({
  rule,
  lane: "127.0.0.1:5080",
  detail,
  advisory: false,
  gating: true,
  ...(offending === undefined ? {} : { offending })
})

const died: Bundle.Failure = {
  failure: "unmatched-datagram",
  step: "s14",
  leg: "A",
  gated_on: { kind: "response", status: 200, cseq_method: "INVITE" },
  reason: "gated on early dialog \"f5\", which must be a dialog of its own",
  arrived: { kind: "response", status: 200, reason: "OK", cseq_method: "INVITE", cseq: 1 }
}

const timedOut: Bundle.Failure = {
  failure: "expect-timed-out",
  step: "s9",
  leg: "B",
  gated_on: "BYE",
  within_ms: 32_000
}

const checkFailed: Bundle.Failure = {
  failure: "check-failed",
  site: "s3.postcondition",
  field: "cdr.count",
  op: "eq",
  expected: "1",
  observed: "0"
}

const verdict = (
  failures: ReadonlyArray<Bundle.Failure>,
  status: Bundle.VerdictStatus = "failed"
): Bundle.RunVerdict => ({
  case: "capture_x.pcap.gz-case1",
  lane: "fake",
  status,
  failures,
  ...(failures.length === 0 ? {} : { failed_step: "s14" })
})

const failed = { passed: false, skipped: null }

const bodyRow = (over: Partial<Confrontation.ConfrontationRecord> = {}): Confrontation.ConfrontationRecord => ({
  lane: "fake",
  capture: "capture_x.pcap.gz",
  case: "capture_x.pcap.gz-case1",
  run: 0,
  step: "s29",
  kind: "body",
  signature: "body:sdp:m0:a=ptime:initial-invite",
  name: "application/sdp",
  scope: "initial-invite",
  captured: ["a=ptime:20"],
  replayed: ["a=ptime:30"],
  inbound: false,
  added: [],
  removed: [],
  class: "unknown",
  rule: "",
  ticket: "",
  ...over
})

describe("mainErrorOf", () => {
  it("says nothing of a cell that passed, whatever else it carries", () => {
    expect(mainErrorOf({
      passed: true,
      skipped: null,
      verdict: verdict([died]),
      charged: [finding("rport-echo", "x — RFC 3581 §4 requires it")]
    })).toBeNull()
  })

  it("says nothing of a cell the lane skipped", () => {
    expect(mainErrorOf({ passed: false, skipped: "blocked:no-mrf", charged: [] })).toBeNull()
  })

  it("leads with the interpreter's refusal, whatever the audit or the verdict say", () => {
    expect(mainErrorOf({
      ...failed,
      error: "replay exited 2 (refused) and left no bundle:\nrefused: lint refuses this lane",
      verdict: verdict([died]),
      charged: [finding("rport-echo", "x — RFC 3581 §4")]
    })).toEqual({
      kind: "refused",
      rule: "interpreter-refused",
      rfc: null,
      step: null,
      expected: null,
      detail: "replay exited 2 (refused) and left no bundle:"
    })
  })

  it("leads with a charge over the step the run died at", () => {
    const error = mainErrorOf({
      ...failed,
      verdict: verdict([died]),
      charged: [finding(
        "unsupported-415-accepts",
        "Sent 415 response carries no Accept header — {Uas} RFC 3261 §8.2.3 / RFC3261-MUST-036",
        7
      )]
    })
    expect(error).toEqual({
      kind: "rfc",
      rule: "unsupported-415-accepts",
      rfc: "RFC 3261 §8.2.3",
      step: null,
      expected: null,
      detail: "Sent 415 response carries no Accept header — {Uas} RFC 3261 §8.2.3 / RFC3261-MUST-036"
    })
  })

  it("picks the charge on the earliest offending message, an unplaced one last", () => {
    const error = mainErrorOf({
      ...failed,
      charged: [
        finding("later", "later — RFC 3261 §12", 9),
        finding("unplaced", "no offending index"),
        finding("earlier", "earlier — RFC 3262 §3", 4)
      ]
    })
    expect(error?.rule).toEqual("earlier")
    expect(error?.rfc).toEqual("RFC 3262 §3")

    const onlyUnplaced = mainErrorOf({
      ...failed,
      charged: [finding("unplaced", "no citation at all")]
    })
    expect(onlyUnplaced?.rule).toEqual("unplaced")
    expect(onlyUnplaced?.rfc).toBeNull()
  })

  it("names the step the run died at and the message it was waiting for", () => {
    expect(mainErrorOf({ ...failed, verdict: verdict([died]), charged: [] })).toEqual({
      kind: "died",
      rule: null,
      rfc: null,
      step: "s14",
      expected: "200 to INVITE",
      detail: "gated on early dialog \"f5\", which must be a dialog of its own"
    })
  })

  it("reads an expect that timed out as a death on that step", () => {
    const error = mainErrorOf({ ...failed, verdict: verdict([timedOut]), charged: [] })
    expect(error?.kind).toEqual("died")
    expect(error?.step).toEqual("s9")
    expect(error?.expected).toEqual("BYE")
    expect(error?.detail).toContain("32000 ms")
  })

  it("prefers the death over an earlier failure of another kind", () => {
    const error = mainErrorOf({ ...failed, verdict: verdict([checkFailed, died]), charged: [] })
    expect(error?.kind).toEqual("died")
    expect(error?.step).toEqual("s14")
  })

  it("falls back to the first failure, under its own tag, where nothing died waiting", () => {
    const error = mainErrorOf({ ...failed, verdict: verdict([checkFailed]), charged: [] })
    expect(error?.kind).toEqual("failure")
    expect(error?.rule).toEqual("check-failed")
    expect(error?.step).toBeNull()
    expect(error?.expected).toBeNull()
    expect(error?.detail).not.toContain("\n")
    expect(error?.detail).toContain("cdr.count")
  })

  it("names the step of a failure that has one", () => {
    const sendFailed: Bundle.Failure = { failure: "send-failed", step: "s2", leg: "A", detail: "EPIPE" }
    const error = mainErrorOf({ ...failed, verdict: verdict([sendFailed]), charged: [] })
    expect(error?.rule).toEqual("send-failed")
    expect(error?.step).toEqual("s2")
    expect(error?.detail).toEqual("EPIPE")
  })

  it("says so when a failing cell has no verdict to read", () => {
    const error = mainErrorOf({ ...failed, charged: [] })
    expect(error?.kind).toEqual("failure")
    expect(error?.rule).toEqual("no-verdict")
  })

  it("names the confrontation records when the verdict is clean and the cell still did not pass", () => {
    const error = mainErrorOf({
      ...failed,
      verdict: verdict([], "ok"),
      charged: [],
      confrontation: { unknown: 2, unlisted: 1 }
    })
    expect(error?.kind).toEqual("failure")
    expect(error?.rule).toEqual("reclassified")
    expect(error?.detail).toEqual("confrontation: 2 unknown, 1 unlisted")
    // No classification to cite: the detail still says which tier answered.
    expect(mainErrorOf({ ...failed, verdict: verdict([], "ok"), charged: [] })?.detail)
      .toEqual("verdict ok with no failure named and no classification")
  })

  describe("a body row no rule accepts", () => {
    const clean = { ...failed, verdict: verdict([], "ok"), charged: [], confrontation: { unknown: 1, unlisted: 0 } }

    it("is bad-body at the row's step, the signature and the captured → replayed line its detail", () => {
      expect(mainErrorOf({ ...clean, body: [bodyRow(), bodyRow({ step: "s43" })] })).toEqual({
        kind: "bad-body",
        rule: "bad-body",
        rfc: null,
        step: "s29",
        expected: null,
        detail: "body:sdp:m0:a=ptime:initial-invite: a=ptime:20 → a=ptime:30"
      })
    })

    it("names the first line that differs of a whole-document row", () => {
      const bytes = bodyRow({
        signature: "body:sdp:document:bytes:initial-invite",
        captured: ["v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=-\r\na=ptime:20\r\na=sendrecv\r\n"],
        replayed: ["v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=-\r\na=sendrecv\r\na=ptime:20\r\n"]
      })
      expect(mainErrorOf({ ...clean, body: [bytes] })?.detail)
        .toEqual("body:sdp:document:bytes:initial-invite: a=ptime:20 → a=sendrecv")
    })

    it("names a side that carries no line as (none), and an unattributed row has no step", () => {
      const dropped = bodyRow({ step: "", signature: "body:sdp:m0:a=fmtp:initial-invite", captured: ["a=fmtp:101 0-15"], replayed: [] })
      const error = mainErrorOf({ ...clean, body: [dropped] })
      expect(error?.step).toBeNull()
      expect(error?.detail).toEqual("body:sdp:m0:a=fmtp:initial-invite: a=fmtp:101 0-15 → (none)")
    })

    it("comes after the refusal, a charge, a death and the verdict's own failures", () => {
      const body = [bodyRow()]
      expect(mainErrorOf({ ...failed, error: "refused", verdict: verdict([]), charged: [], body })?.kind).toEqual("refused")
      expect(mainErrorOf({ ...failed, verdict: verdict([], "ok"), charged: [finding("r", "x — RFC 3261 §8")], body })?.kind)
        .toEqual("rfc")
      expect(mainErrorOf({ ...failed, verdict: verdict([died]), charged: [], body })?.kind).toEqual("died")
      expect(mainErrorOf({ ...failed, verdict: verdict([checkFailed]), charged: [], body })?.rule).toEqual("check-failed")
    })

    it("leads where the body gate is what refused a cell whose failures a named rule accepts", () => {
      const body = [bodyRow()]
      expect(mainErrorOf({ ...failed, verdict: verdict([died]), charged: [], body, bodyGated: true })?.kind)
        .toEqual("bad-body")
      expect(mainErrorOf({ ...failed, verdict: verdict([checkFailed]), charged: [], body, bodyGated: true })?.rule)
        .toEqual("bad-body")
      // Never over the interpreter's refusal or a charge that stands.
      expect(mainErrorOf({ ...failed, error: "refused", charged: [], body, bodyGated: true })?.kind).toEqual("refused")
      expect(
        mainErrorOf({ ...failed, verdict: verdict([died]), charged: [finding("r", "x — RFC 3261 §8")], body, bodyGated: true })
          ?.kind
      ).toEqual("rfc")
    })

    it("names what a whole-document row differs by where every line is the same", () => {
      const endings = bodyRow({
        signature: "body:sdp:document:bytes:initial-invite",
        captured: ["v=0\r\ns=-\r\n"],
        replayed: ["v=0\ns=-\n"]
      })
      expect(mainErrorOf({ ...clean, body: [endings] })?.detail)
        .toEqual("body:sdp:document:bytes:initial-invite: line endings differ (CRLF → LF)")
      const blank = bodyRow({
        signature: "body:sdp:document:bytes:initial-invite",
        captured: ["v=0\r\ns=-\r\n"],
        replayed: ["v=0\r\n\r\ns=-\r\n"]
      })
      expect(mainErrorOf({ ...clean, body: [blank] })?.detail)
        .toEqual("body:sdp:document:bytes:initial-invite: blank lines differ")
    })

    it("is bad-body for a body of any media type, the first differing line its detail", () => {
      const dataBody = bodyRow({
        step: "s33",
        signature: "body:application/vnd.example.data:request:BYE:in-dialog",
        name: "application/vnd.example.data",
        scope: "request:BYE:in-dialog",
        captured: ["dwdHAIIDjQEB"],
        replayed: [""]
      })
      expect(mainErrorOf({ ...clean, body: [dataBody] })).toEqual({
        kind: "bad-body",
        rule: "bad-body",
        rfc: null,
        step: "s33",
        expected: null,
        detail: "body:application/vnd.example.data:request:BYE:in-dialog: dwdHAIIDjQEB → (none)"
      })
    })

    it("comes before reclassified: a clean verdict whose only reason is a body row reads bad-body", () => {
      expect(mainErrorOf({ ...clean, body: [bodyRow()] })?.rule).toEqual("bad-body")
      expect(mainErrorOf({ ...clean, body: [] })?.rule).toEqual("reclassified")
    })
  })

  it("survives a death the contract could not decode: no gate, no reason", () => {
    const bare = { failure: "unmatched-datagram", step: "s3", leg: "A" } as unknown as Bundle.Failure
    expect(mainErrorOf({ ...failed, verdict: verdict([bare]), charged: [] })).toEqual({
      kind: "died",
      rule: null,
      rfc: null,
      step: "s3",
      expected: null,
      detail: ""
    })
  })
})

describe("citation", () => {
  it("takes the first RFC the detail cites, section and all", () => {
    expect(citation("x — RFC 3581 §4 requires the server to set it; see RFC 3261 §18")).toEqual("RFC 3581 §4")
    expect(citation("dotted RFC 3261 §17.2.1 section")).toEqual("RFC 3261 §17.2.1")
    expect(citation("no section: RFC 3262 says so")).toEqual("RFC 3262")
    expect(citation("plain words")).toBeNull()
    // A sentence's full stop is not part of the section.
    expect(citation("required by RFC 3261 §4. The server then")).toEqual("RFC 3261 §4")
    expect(citation("RFC 3261 §17.2.1. Next")).toEqual("RFC 3261 §17.2.1")
  })
})

describe("gatedOnText", () => {
  it("spells the three gates as the run does", () => {
    expect(gatedOnText({ kind: "request", method: "BYE" })).toEqual("BYE")
    expect(gatedOnText({ kind: "response", status: 200, cseq_method: "INVITE" })).toEqual("200 to INVITE")
    expect(gatedOnText({ kind: "response", status: 200 })).toEqual("200")
  })

  it("keeps a timed-out expect's own string", () => {
    expect(gatedOnText("180 to INVITE")).toEqual("180 to INVITE")
  })
})

describe("failingBody", () => {
  it("keeps every body row no rule accepts, whatever its media type, in record order", () => {
    const sdp = bodyRow()
    const dataBody = bodyRow({ signature: "body:application/vnd.example.data:request:BYE:in-dialog" })
    const multipart = bodyRow({ signature: "body:multipart/mixed:initial-invite", class: "unlisted" })
    const accepted = bodyRow({ signature: "body:sdp:session:o=:initial-invite", class: "accepted" })
    const knownBug = bodyRow({ signature: "body:application/isup:initial-invite", class: "known-bug" })
    const header = bodyRow({ kind: "header", signature: "header:allow:initial-invite" })
    expect(failingBody([sdp, accepted, dataBody, header, knownBug, multipart])).toEqual([sdp, dataBody, multipart])
  })
})
