/**
 * The post-run escape hatch: a structural failure whose every divergence is a
 * rule-classified status substitution restates as passed ("ok with accepted
 * delta") — and NOTHING else is touched.
 */
import { NodeFileSystem } from "@effect/platform-node"
import { Bundle, Confrontation } from "@sip/contracts"
import { Reclassifier } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as BlessAudit from "../src/audit.js"
import { reclassifierLayer } from "../src/reclassify.js"

const record = (over: Partial<Confrontation.ConfrontationRecord>): Confrontation.ConfrontationRecord => ({
  lane: "lab",
  capture: "cap.pcap.gz",
  case: "cap-case1",
  run: 0,
  step: "s7",
  kind: "shape",
  signature: "shape:status-substitution:503->500:initial-invite",
  name: "",
  scope: "initial-invite",
  captured: ["status-substitution:503->500"],
  replayed: [],
  inbound: false,
  added: [],
  removed: [],
  class: "accepted",
  rule: "final-class-kept",
  ticket: "",
  ...over
})

const cellDir = (records: ReadonlyArray<Confrontation.ConfrontationRecord>): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "reclassify-"))
  fs.writeFileSync(
    path.join(dir, "confrontation.ndjson"),
    records.map((r) => `${Confrontation.emitConfrontationRecord(r)}\n`).join("")
  )
  return dir
}

const unmatched = {
  failure: "unmatched-datagram" as const,
  step: "s7",
  leg: "A",
  gated_on: { kind: "response" as const, status: 503 },
  reason: "gated on response 503; 500 arrived",
  arrived: {
    kind: "response" as const,
    status: 500,
    reason: "Internal Server Error",
    cseq_method: "INVITE",
    cseq: 1
  }
}

const outcome = (over: Partial<Reclassifier.RunOutcome>): Reclassifier.RunOutcome => ({
  caseId: "cap-case1",
  lane: "lab",
  verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched] },
  dir: "/nowhere",
  passed: false,
  ...over
})

/** The deployment's reclassifier over the named audit sink and the real file system, asked about one cell. */
const reclassifyInto = (
  sink: Layer.Layer<BlessAudit.Sink>,
  o: Reclassifier.RunOutcome
): Promise<Reclassifier.Reclassified> =>
  Effect.runPromise(
    Effect.gen(function* () {
      const service = yield* Reclassifier.Service
      return yield* service.reclassify(o)
    }).pipe(
      Effect.provide(Layer.provide(reclassifierLayer, sink)),
      Effect.provide(NodeFileSystem.layer)
    )
  )

const reclassify = (o: Reclassifier.RunOutcome): Promise<Reclassifier.Reclassified> =>
  reclassifyInto(BlessAudit.Sink.silent, o)

/** A sink that keeps what it is given, so a test reads the audit back without a file. */
const collecting = (): { readonly layer: Layer.Layer<BlessAudit.Sink>; readonly filed: Array<BlessAudit.AuditRecord> } => {
  const filed: Array<BlessAudit.AuditRecord> = []
  const layer = Layer.succeed(
    BlessAudit.Sink,
    BlessAudit.Sink.of({
      append: (records) => Effect.sync(() => void filed.push(...records)),
      read: () => Effect.sync(() => [...filed])
    })
  )
  return { layer, filed }
}

/** A sink whose log cannot be written. */
const unwritable: Layer.Layer<BlessAudit.Sink> = Layer.succeed(
  BlessAudit.Sink,
  BlessAudit.Sink.of({
    append: () => Effect.fail(new BlessAudit.AuditLogUnreachable({ op: "append", path: "/nowhere/log", cause: "EACCES" })),
    read: () => Effect.succeed([])
  })
)

const OFFENDING = `v=0\r\no=- 1 1 IN IP4 198.51.100.1\r\ns=-\r\nm=image 0 udptl\r\n`

const charge = (over: Partial<Bundle.RfcFinding> = {}): Bundle.RfcFinding => ({
  rule: "answer-media-type-matches-offer",
  lane: "203.0.113.1:5080",
  detail: `m=[0] offered 'audio', answered 'image' (callId b-1@203.0.113.1, CSeq 1)`,
  advisory: false,
  gating: true,
  charged: "203.0.113.1:5080",
  ...over
})

const committed: Bundle.RfcFinding = charge({
  detail: `m=[0] offered 'audio', answered 'image' (callId A-9@pivot.invalid, CSeq 6)`,
  gating: false,
  charged: "203.0.113.1:5060",
  actor: "ep0"
})

const message = (dir: Bundle.Dir, callId: string, body: string): Bundle.RecordedMessage => ({
  seq: 1,
  dir,
  at_us: 0,
  raw: `SIP/2.0 200 OK\r\nCall-ID: ${callId}\r\nCSeq: 1 INVITE\r\nContent-Length: ${body.length}\r\n\r\n${body}`
})

/** A cell dir carrying its confrontation, its audit and its per-leg recording. */
const audited = (
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  findings: ReadonlyArray<Bundle.RfcFinding>,
  messages: ReadonlyArray<Bundle.RecordedMessage>
): string => {
  const dir = cellDir(records)
  fs.writeFileSync(
    path.join(dir, "rfc.json"),
    Bundle.emitRunRfcAudit({ status: "audited", findings })
  )
  fs.mkdirSync(path.join(dir, "recording"))
  fs.writeFileSync(
    path.join(dir, "recording", "A.jsonl"),
    messages.map((m) => `${Bundle.emitRecordedMessage(m)}\n`).join("")
  )
  return dir
}

const relayed = [
  message("out", "A-9@pivot.invalid", OFFENDING),
  message("in", "b-1@203.0.113.1", OFFENDING)
]

/** A cell dir whose one gating charge reads back to a peer: the RFC ruling inherits it. */
const inherited = (records: ReadonlyArray<Confrontation.ConfrontationRecord>): string =>
  audited(records, [charge(), committed], relayed)

describe("the reclassifier", () => {
  it("restates a run failed only on rule-accepted substitutions, with the rules named", async () => {
    const dir = cellDir([record({})])
    const got = await reclassify(outcome({ dir }))
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("final-class-kept")
  })

  it("counts the cell off the same reading: the rule under its class and `shape`, the bless it carried", async () => {
    const dir = cellDir([record({}), record({ kind: "header", signature: "header:Via", rule: "via-stack-owned" })])
    const got = await reclassify(outcome({ dir }))
    expect(got.passed).toBe(true)
    expect(got.hits).toEqual([
      { family: "tolerance", rule: "final-class-kept", bucket: "accepted", hits: 1 },
      { family: "tolerance", rule: "final-class-kept", bucket: "shape", hits: 1 },
      { family: "tolerance", rule: "via-stack-owned", bucket: "accepted", hits: 1 },
      { family: "bless", rule: "bless-premise", bucket: "fired", hits: 1 },
      { family: "tolerance", rule: "final-class-kept", bucket: "blessed", hits: 1 }
    ])
  })

  it("counts a record no rule names as a hole, and a green cell's stray shape against the audit", async () => {
    const dir = cellDir([record({}), record({ kind: "header", signature: "header:Allow", rule: "", class: "unknown" })])
    const got = await reclassify(
      outcome({ dir, passed: true, verdict: { case: "cap-case1", lane: "lab", status: "ok" } })
    )
    expect(got.passed).toBe(true)
    expect(got.hits).toContainEqual({ family: "tolerance", rule: "(unnamed)", bucket: "unknown", hits: 1 })
    expect(got.hits).toContainEqual({
      family: "bless",
      rule: "failure-shape-on-a-green-cell",
      bucket: "fired",
      hits: 1
    })
    expect(got.hits?.some((h) => h.rule === "bless-premise")).toBe(false)
  })

  it("counts nothing where there is no sheet to count", async () => {
    expect((await reclassify(outcome({}))).hits).toBeUndefined()
    expect((await reclassify(outcome({ passed: true }))).hits).toBeUndefined()
  })

  it("an unclassified record leaves the failure standing", async () => {
    const dir = cellDir([record({ class: "unknown", rule: "" })])
    expect((await reclassify(outcome({ dir }))).passed).toBe(false)
  })

  describe("a body row no rule accepts fails its cell", () => {
    const bodyRow = record({
      kind: "body",
      name: "application/sdp",
      signature: "body:sdp:m0:a=rtpmap:response:200:INVITE",
      scope: "response:200:INVITE",
      captured: ["a=rtpmap:8 PCMA/8000"],
      replayed: ["a=rtpmap:0 PCMU/8000"],
      class: "unknown",
      rule: ""
    })
    const ok = { case: "cap-case1", lane: "lab", status: "ok" as const }

    it("a cell the interpreter passed is failed by it, the row named, the counts kept", async () => {
      const dir = cellDir([bodyRow])
      const got = await reclassify(outcome({ dir, passed: true, verdict: ok }))
      expect(got.passed).toBe(false)
      expect(got.reason).toContain("bad-body")
      expect(got.reason).toContain("body:sdp:m0:a=rtpmap:response:200:INVITE")
      expect(got.hits).toContainEqual({ family: "tolerance", rule: "(unnamed)", bucket: "unknown", hits: 1 })
    })

    it("names the row a rule names ahead of an unnamed one, the rule with it", async () => {
      const named = {
        ...bodyRow,
        step: "s9",
        signature: "body:sdp:session:o=:request:ACK:in-dialog",
        class: "unlisted" as const,
        rule: "sdp-origin-not-continued"
      }
      const got = await reclassify(outcome({ dir: cellDir([bodyRow, named]), passed: true, verdict: ok }))
      expect(got.passed).toBe(false)
      expect(got.reason).toContain("first sdp-origin-not-continued: body:sdp:session:o=:request:ACK:in-dialog")
    })

    it("an accepted session-description row decides nothing", async () => {
      const accepted = { ...bodyRow, signature: "body:sdp:session:o=:initial-invite", class: "accepted" as const, rule: "sdp-origin-stack-owned" }
      const got = await reclassify(outcome({ dir: cellDir([accepted]), passed: true, verdict: ok }))
      expect(got.passed).toBe(true)
    })

    it("an unknown row of another kind leaves a passed cell as the interpreter said", async () => {
      const header = record({ kind: "header", signature: "header:Allow", rule: "", class: "unknown" })
      expect((await reclassify(outcome({ dir: cellDir([header]), passed: true, verdict: ok }))).passed).toBe(true)
    })

    const gate = { family: "bless", rule: "bad-body", bucket: "fired", hits: 1 }

    it("the bless does not fire where the only unknown record beside the substitution is a `body:` row", async () => {
      const got = await reclassify(outcome({ dir: cellDir([record({}), bodyRow]) }))
      expect(got.passed).toBe(false)
    })

    it("names the gate where the body row alone kept the bless from a failed cell, and counts it", async () => {
      const got = await reclassify(outcome({ dir: cellDir([record({}), bodyRow]) }))
      expect(got.reason).toContain("bad-body")
      expect(got.hits).toContainEqual(gate)
      const green = await reclassify(outcome({ dir: cellDir([bodyRow]), passed: true, verdict: ok }))
      expect(green.hits).toContainEqual(gate)
    })

    it("names the gate on a clean verdict an enforcing run already failed on the body row", async () => {
      const got = await reclassify(outcome({ dir: cellDir([bodyRow]), passed: false, verdict: ok }))
      expect(got.passed).toBe(false)
      expect(got.reason).toContain("bad-body")
      expect(got.hits).toContainEqual(gate)
    })

    it("does not name the gate where the cell stays red without the body row", async () => {
      const header = record({ kind: "header", signature: "header:Allow", rule: "", class: "unknown" })
      const got = await reclassify(outcome({ dir: cellDir([record({}), bodyRow, header]) }))
      expect(got.passed).toBe(false)
      expect(got.reason).toBeUndefined()
      expect(got.hits).not.toContainEqual(gate)
    })

    it("the reason names the earliest step of the document, whatever order the rows were written in", async () => {
      const dir = cellDir([{ ...bodyRow, step: "s7" }, { ...bodyRow, step: "s3", signature: "body:sdp:m0:a=direction:initial-invite" }])
      fs.writeFileSync(
        path.join(dir, "pivot.json"),
        JSON.stringify({ flow: [{ id: "s3", op: "expect" }, { id: "s7", op: "expect" }] })
      )
      const got = await reclassify(outcome({ dir, passed: true, verdict: ok }))
      expect(got.reason).toContain("first body:sdp:m0:a=direction:initial-invite at s3")
    })

    it("a failure-less cell with an inherited RFC charge does not restate over the row", async () => {
      const settled = { case: "cap-case1", lane: "lab", status: "ok" as const }
      expect((await reclassify(outcome({ dir: inherited([]), verdict: settled }))).passed).toBe(true)
      expect((await reclassify(outcome({ dir: inherited([bodyRow]), verdict: settled }))).passed).toBe(false)
    })

    it("an unknown body row under another type blocks the bless and names the gate", async () => {
      const dir = cellDir([
        record({}),
        record({ ...bodyRow, name: "application/example+xml", signature: "body:application/example+xml:request:INFO:in-dialog" })
      ])
      const got = await reclassify(outcome({ dir }))
      expect(got.passed).toBe(false)
      expect(got.reason).toContain("bad-body")
      expect(got.hits).toContainEqual(gate)
    })

    it("a non-SDP body row fails a cell the interpreter passed, the row named", async () => {
      const opaque = record({
        kind: "body",
        name: "application/vnd.example.data",
        signature: "body:application/vnd.example.data:request:BYE:in-dialog",
        scope: "request:BYE:in-dialog",
        step: "s33",
        captured: ["dwdHAIIDjQEB"],
        replayed: [""],
        class: "unknown",
        rule: ""
      })
      const multipart = record({
        kind: "body",
        name: "multipart/mixed",
        signature: "body:multipart/mixed:initial-invite",
        scope: "initial-invite",
        step: "s3",
        captured: ["application/sdp", "application/vnd.example.data"],
        replayed: [],
        class: "unknown",
        rule: ""
      })
      const dir = cellDir([opaque, multipart])
      fs.writeFileSync(
        path.join(dir, "pivot.json"),
        JSON.stringify({ flow: [{ id: "s3", op: "expect" }, { id: "s33", op: "expect" }] })
      )
      const got = await reclassify(outcome({ dir, passed: true, verdict: ok }))
      expect(got.passed).toBe(false)
      expect(got.reason).toContain("bad-body: 2 body row(s) no rule accepts, first body:multipart/mixed:initial-invite at s3")
      expect(got.hits).toContainEqual(gate)
    })

    it("an accepted non-SDP body row decides nothing", async () => {
      const accepted = record({
        kind: "body",
        name: "application/example+xml",
        signature: "body:application/example+xml:request:INFO:in-dialog",
        class: "accepted",
        rule: "some-rule"
      })
      expect((await reclassify(outcome({ dir: cellDir([accepted]), passed: true, verdict: ok }))).passed).toBe(true)
    })
  })

  it("a failure kind the confrontation does not cover leaves the failure standing", async () => {
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({
        dir,
        verdict: {
          case: "c",
          lane: "lab",
          status: "failed",
          failures: [unmatched, { failure: "cdr-mismatch", expected: "1 record", observed: "0 records" }]
        }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("never forgives a post-run cleanup breach, whatever the sheet says", async () => {
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({
        dir,
        verdict: {
          case: "cap-case1",
          lane: "lab",
          status: "failed",
          failures: [
            {
              failure: "check-failed",
              site: "post-run cleanup",
              field: "sut.reaped",
              op: "reaped",
              expected: "every call removed, no lock, stamp, mark or dropped INVITE final left",
              observed: "call leak: creations (2) != removals (1)"
            }
          ]
        }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("leaves a count standing: the interpreter states which ladder it asserted", async () => {
    const dir = cellDir([record({ kind: "header", signature: "header:Allow", rule: "", class: "accepted" })])
    const got = await reclassify(
      outcome({
        dir,
        verdict: {
          case: "cap-case1",
          lane: "lab",
          status: "failed",
          failures: [
            { failure: "retransmit-count-mismatch", step: "s45", leg: "A", declared: 1, expected: 1, observed: 2 }
          ]
        }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("a glare's two refusals at one step are covered by a substitution and a stray", async () => {
    const stray = {
      failure: "unmatched-datagram" as const,
      step: "s7",
      leg: "A",
      gated_on: { kind: "response" as const, status: 503 },
      reason: "gated on response 503; a BYE request arrived",
      arrived: { kind: "request" as const, method: "BYE", cseq: 5 }
    }
    const dir = cellDir([
      record({}),
      record({
        signature: "shape:serviced-stray:BYE:auto-reacted",
        scope: "",
        captured: ["serviced-stray:BYE:auto-reacted"],
        rule: "teardown-cross-stray"
      })
    ])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched, stray] }
      })
    )
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("teardown-cross-stray")
    expect(got.reason).toContain("final-class-kept")
  })

  it("an omitted 100 is covered by the omission record at its own step", async () => {
    const omitted = {
      failure: "trying-omitted" as const,
      step: "s13",
      leg: "B",
      arrived: { kind: "response" as const, status: 487, reason: "Request Terminated", cseq_method: "INVITE", cseq: 2 },
      after_ms: 2
    }
    const omission = record({
      step: "s13",
      signature: "shape:trying-omitted:response:100:INVITE",
      scope: "response:100:INVITE",
      captured: ["100"],
      replayed: ["487 after 2 ms"],
      rule: "trying-omitted-before-final"
    })
    const verdict = { case: "cap-case1", lane: "lab", status: "failed" as const, failures: [omitted, unmatched] }
    const got = await reclassify(outcome({ dir: cellDir([record({}), omission]), verdict }))
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("trying-omitted-before-final")

    const elsewhere = await reclassify(outcome({ dir: cellDir([record({}), { ...omission, step: "s9" }]), verdict }))
    expect(elsewhere.passed, "an omission at another step answers nothing").toBe(false)

    const stray = { ...omission, signature: "shape:extra-message:487", scope: "", captured: ["extra-message:487"], replayed: [] }
    const other = await reclassify(outcome({ dir: cellDir([record({}), stray]), verdict }))
    expect(other.passed, "only the omission states the omission").toBe(false)
  })

  it("an expect that timed out is covered by the missing message at its own step", async () => {
    const absent = {
      failure: "expect-timed-out" as const,
      step: "s22",
      leg: "A",
      gated_on: "BYE",
      within_ms: 37000
    }
    const dir = cellDir([
      record({}),
      record({
        step: "s22",
        signature: "shape:missing-message:BYE",
        scope: "",
        captured: ["missing-message:BYE"],
        rule: "teardown-cross-unrelayed"
      })
    ])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched, absent] }
      })
    )
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("teardown-cross-unrelayed")
  })

  it("an expect that timed out is NOT covered by a stray at that step, which states no absence", async () => {
    const absent = {
      failure: "expect-timed-out" as const,
      step: "s22",
      leg: "A",
      gated_on: "BYE",
      within_ms: 37000
    }
    const dir = cellDir([
      record({
        step: "s22",
        signature: "shape:serviced-stray:BYE:auto-reacted",
        scope: "",
        captured: ["serviced-stray:BYE:auto-reacted"],
        rule: "teardown-cross-stray"
      })
    ])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [absent] }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("a stray that beat every expect to the leg is covered by a stepless record", async () => {
    const early = {
      failure: "unexpected-datagram" as const,
      leg: "A",
      arrived: { kind: "request" as const, method: "BYE", cseq: 5 }
    }
    const dir = cellDir([
      record({}),
      record({
        step: "",
        signature: "shape:serviced-stray:BYE:auto-reacted",
        scope: "",
        captured: ["serviced-stray:BYE:auto-reacted"],
        rule: "teardown-cross-stray"
      })
    ])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched, early] }
      })
    )
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("teardown-cross-stray")
  })

  it("a stray that arrived once the flow had COMPLETED is covered the same way", async () => {
    const past = {
      failure: "datagram-after-flow" as const,
      leg: "D",
      arrived: { kind: "request" as const, method: "ACK", cseq: 1 }
    }
    const dir = cellDir([
      record({
        step: "",
        signature: "shape:extra-message:ACK",
        scope: "",
        captured: ["extra-message:ACK"],
        rule: "ack-completes-unscripted-final"
      })
    ])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [past] }
      })
    )
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("ack-completes-unscripted-final")
  })

  it("a stray past the flow that no record names leaves the failure standing", async () => {
    const past = {
      failure: "datagram-after-flow" as const,
      leg: "D",
      arrived: { kind: "request" as const, method: "ACK", cseq: 1 }
    }
    const dir = cellDir([record({ step: "", signature: "shape:extra-message:ACK", scope: "", class: "unknown", rule: "" })])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [past] }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("a stray with no record of its own is not covered", async () => {
    const early = {
      failure: "unexpected-datagram" as const,
      leg: "A",
      arrived: { kind: "request" as const, method: "BYE", cseq: 5 }
    }
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched, early] }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("a refusal whose step carries no shape record is not covered", async () => {
    const elsewhere = {
      failure: "unmatched-datagram" as const,
      step: "s9",
      leg: "A",
      gated_on: { kind: "response" as const, status: 503 },
      reason: "gated on response 503; a BYE request arrived",
      arrived: { kind: "request" as const, method: "BYE", cseq: 5 }
    }
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({
        dir,
        verdict: { case: "cap-case1", lane: "lab", status: "failed", failures: [unmatched, elsewhere] }
      })
    )
    expect(got.passed).toBe(false)
  })

  it("a header record nothing named still leaves the failure standing", async () => {
    const dir = cellDir([
      record({}),
      record({ kind: "header", name: "Contact", signature: "header:contact:response:200:BYE", class: "unknown", rule: "" })
    ])
    expect((await reclassify(outcome({ dir }))).passed).toBe(false)
  })

  it("a missing record file leaves the failure standing", async () => {
    expect((await reclassify(outcome({}))).passed).toBe(false)
  })

  it("a sheet that will not parse leaves the cell as the interpreter said, and counts nothing", async () => {
    const dir = cellDir([])
    fs.writeFileSync(path.join(dir, "confrontation.ndjson"), "{ not a record\n")
    expect(await reclassify(outcome({ dir }))).toEqual({ passed: false })
    expect(await reclassify(outcome({ dir, passed: true }))).toEqual({ passed: true })
  })

  it("a passing run passes through untouched", async () => {
    const got = await reclassify(outcome({ passed: true }))
    expect(got).toEqual({ passed: true })
  })
})

/**
 * The bless put to the population it claims not to apply to. The
 * cell's verdict is never touched by any of it: what the audit answers is
 * whether a RULE is written narrowly enough.
 */
describe("the bless audit on a passing cell", () => {
  const audited = async (
    records: ReadonlyArray<Confrontation.ConfrontationRecord>,
    over: Partial<Reclassifier.RunOutcome> = {}
  ): Promise<{ readonly passed: boolean; readonly findings: ReadonlyArray<BlessAudit.AuditRecord> }> => {
    const sink = collecting()
    const got = await reclassifyInto(
      sink.layer,
      outcome({
        dir: cellDir(records),
        passed: true,
        verdict: { case: "cap-case1", lane: "lab", status: "ok" },
        ...over
      })
    )
    return { passed: got.passed, findings: sink.filed }
  }

  it("finds nothing on a cell whose confrontation is headers alone", async () => {
    const got = await audited([record({ kind: "header", signature: "header:contact:response:200:INVITE" })])
    expect(got.passed).toBe(true)
    expect(got.findings).toEqual([])
  })

  it("lets the held-final cadence through: it is derived from the recording, not a failure", async () => {
    const got = await audited([record({ signature: "shape:retransmission:INVITE:2xx", rule: "held-final-retransmission-cadence" })])
    expect(got.findings).toEqual([])
  })

  it("files a failure-derived shape on a green cell against the rule that classified it", async () => {
    const got = await audited([record({})])
    expect(got.passed).toBe(true)
    expect(got.findings).toHaveLength(1)
    expect(got.findings[0]!.finding).toBe("failure-shape-on-a-green-cell")
    expect(got.findings[0]!.rules).toEqual(["final-class-kept"])
    expect(got.findings[0]!.signatures).toEqual(["shape:status-substitution:503->500:initial-invite"])
  })

  it("files nothing through the silent sink and leaves the cell green", async () => {
    const dir = cellDir([record({})])
    expect(
      await reclassify(outcome({ dir, passed: true, verdict: { case: "c", lane: "lab", status: "ok" } }))
    ).toMatchObject({ passed: true })
  })

  it("leaves the cell as the interpreter said it where the sink cannot be written", async () => {
    const dir = cellDir([record({})])
    expect(
      await reclassifyInto(
        unwritable,
        outcome({ dir, passed: true, verdict: { case: "c", lane: "lab", status: "ok" } })
      )
    ).toMatchObject({ passed: true })
  })

  it("aggregates by rule, not by cell", async () => {
    const rows: ReadonlyArray<BlessAudit.AuditRecord> = [
      { finding: "failure-shape-on-a-green-cell", lane: "lab", rules: ["r1"], signatures: [], caseId: "a", detail: "" },
      { finding: "failure-shape-on-a-green-cell", lane: "lab", rules: ["r1"], signatures: [], caseId: "b", detail: "" },
      { finding: "failure-shape-on-a-green-cell", lane: "lab", rules: [], signatures: [], caseId: "c", detail: "" }
    ]
    expect(BlessAudit.byRule(rows)).toEqual([
      { rule: "r1", finding: "failure-shape-on-a-green-cell", count: 2, evidence: ["a", "b"] },
      { rule: "(unnamed)", finding: "failure-shape-on-a-green-cell", count: 1, evidence: ["c"] }
    ])
  })

  it("says nothing at all when the audit found nothing", async () => {
    expect(BlessAudit.report([])).toEqual([])
  })
})

/**
 * The post-run RFC ruling wired into the same escape hatch: a
 * gating charge the run reads back to a peer stops failing the cell, and one
 * that stands fails it whatever the verdict side says.
 */
describe("the RFC gate's charges, read back to their author", () => {
  const header = record({ kind: "header", signature: "header:Allow", rule: "", class: "accepted" })
  const settled = { case: "cap-case1", lane: "lab", status: "ok" as const }

  it("restates a cell the RFC gate alone reddened, naming the rule it inherited", async () => {
    const dir = audited([header], [charge(), committed], relayed)
    const got = await reclassify(outcome({ dir, verdict: settled }))
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("answer-media-type-matches-offer")
    expect(got.reason).toContain("rfc-violation-inherited-from-a-peer")
  })

  it("leaves a cell red on a charge no peer committed", async () => {
    const dir = audited([header], [charge()], relayed)
    expect((await reclassify(outcome({ dir, verdict: settled }))).passed).toBe(false)
  })

  it("refuses to green a cell over a standing charge, though every failure is classified", async () => {
    const dir = audited([record({})], [charge()], relayed)
    expect((await reclassify(outcome({ dir }))).passed).toBe(false)
  })

  it("restates a cell over a scripted party's source fault: the bless rules the SUT alone, the fault is counted", async () => {
    const dir = audited([record({})], [{ ...committed, source_fault: true }], relayed)
    const got = await reclassify(outcome({ dir }))
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("final-class-kept")
    expect(got.hits).toContainEqual({
      family: "rfc",
      rule: "answer-media-type-matches-offer",
      bucket: "source-fault",
      hits: 1
    })
  })

  it("restates a cell whose only finding is a scripted party's the reference shows, over classified failures", async () => {
    const dir = audited(
      [record({})],
      [{ ...committed, cancelled_by: "rfc_violations: answer-media-type-matches-offer at s6 by uac1" }],
      relayed
    )
    expect((await reclassify(outcome({ dir }))).passed).toBe(true)
  })

  it("blesses the failures and the charges together where both are answered", async () => {
    const dir = audited([record({})], [charge(), committed], relayed)
    const got = await reclassify(outcome({ dir }))
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("final-class-kept")
    expect(got.reason).toContain("rfc-violation-inherited-from-a-peer")
  })

  it("leaves a red cell neither reading explains exactly as it found it", async () => {
    const dir = audited([header], [], [])
    expect((await reclassify(outcome({ dir, verdict: settled }))).passed).toBe(false)
  })

  it("files a charge standing on a cell that passed anyway", async () => {
    const dir = audited([header], [charge()], relayed)
    const sink = collecting()
    const got = await reclassifyInto(sink.layer, outcome({ dir, passed: true, verdict: settled }))
    expect(got.passed).toBe(true)
    expect(got.reason).toBeUndefined()
    expect(got.hits, "the count is taken off the reading the audit log gets").toContainEqual({
      family: "bless",
      rule: "rfc-charge-on-a-green-cell",
      bucket: "fired",
      hits: 1
    })
    expect(sink.filed.map((f) => f.finding)).toEqual(["rfc-charge-on-a-green-cell"])
    expect(sink.filed[0]!.rules).toEqual(["answer-media-type-matches-offer"])
  })

  it("files a charge it would have read back to a peer on a cell that passed anyway", async () => {
    const dir = audited([header], [charge(), committed], relayed)
    const sink = collecting()
    await reclassifyInto(sink.layer, outcome({ dir, passed: true, verdict: settled }))
    expect(sink.filed.map((f) => f.finding)).toEqual(["rfc-inheritance-on-a-green-cell"])
  })
})

/**
 * A `must_fail` declaration the run abandoned before reaching is untestable, so
 * the cell follows the divergence that actually stopped it. Both clauses gate:
 * the anchor must be PENDING, and the rest of the run must already be accepted.
 */
describe("an unreachable must_fail anchor", () => {
  const declared = (step: string) => ({
    failure: "declared-failure-not-produced" as const,
    declared: "unexpected-ack" as const,
    step,
    detail: `step "${step}" put no datagram on leg "A" this run`
  })

  const abandonedAt = (pending: ReadonlyArray<string>) => ({
    leg: "B",
    step: "s7",
    pending
  })

  const verdict = (
    failures: ReadonlyArray<Bundle.Failure>,
    abandoned?: Bundle.Abandoned
  ): Bundle.RunVerdict => ({
    case: "cap-case1",
    lane: "lab",
    status: "failed",
    failures,
    ...(abandoned === undefined ? {} : { abandoned })
  })

  it("is forgiven beside a divergence a rule names, and the reason names that rule", async () => {
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({ dir, verdict: verdict([unmatched, declared("s99")], abandonedAt(["s98", "s99"])) })
    )
    expect(got.passed).toBe(true)
    expect(got.reason).toContain("final-class-kept")
  })

  it("gates where the anchor RAN, whatever the rest of the run is classified as", async () => {
    const dir = cellDir([record({})])
    const ran = await reclassify(outcome({ dir, verdict: verdict([unmatched, declared("s99")]) }))
    expect(ran.passed, "an anchor that ran states the system's wire behaviour").toBe(false)
    const elsewhere = await reclassify(
      outcome({ dir, verdict: verdict([unmatched, declared("s99")], abandonedAt(["s98"])) })
    )
    expect(elsewhere.passed, "a pending list without the anchor says the run reached it").toBe(false)
  })

  it("gates where a failure beside it is not accepted", async () => {
    const unknown = cellDir([record({ class: "unknown", rule: "" })])
    const got = await reclassify(
      outcome({
        dir: unknown,
        verdict: verdict([unmatched, declared("s99")], abandonedAt(["s99"]))
      })
    )
    expect(got.passed).toBe(false)
  })

  it("gates where it is the ONLY failure, since the bless would name nothing", async () => {
    const dir = cellDir([record({})])
    const got = await reclassify(
      outcome({ dir, verdict: verdict([declared("s99")], abandonedAt(["s99"])) })
    )
    expect(got.passed).toBe(false)
  })
})
