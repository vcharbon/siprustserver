/**
 * Reading a run bundle off disk, over a minimal one in `fixtures/`: every cell's
 * main error is filled from what the cell wrote, and the charges a caller rules
 * on are the ones the reading leads with.
 */
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { Verification } from "@sip/contracts"
import * as Bundle from "../src/bundle.js"

const readOf = (dir: string, options?: Bundle.Options): Promise<Bundle.BundleReading> =>
  Effect.runPromise(Bundle.read(dir, options).pipe(Effect.provide(NodeServices.layer)))

const RUN = path.join(import.meta.dirname, "fixtures", "run-main-error")
const CAPTURE = "capture_main.pcap.gz"

const byCase = (reading: Bundle.BundleReading) =>
  new Map(reading.cases.map((entry) => [entry.caseId.slice(CAPTURE.length + 1), entry]))

describe("Bundle.read", () => {
  it("fills each cell's main error under the precedence", async () => {
    const reading = await readOf(RUN)
    expect(reading.status).toEqual("done")
    expect(reading.lane).toEqual("fake")
    const cells = byCase(reading)
    expect([...cells.keys()]).toEqual([
      "case1",
      "case2",
      "case3",
      "case4",
      "case5",
      "case6",
      "case7",
      "case8",
      "case9",
      "case10",
      "case11",
      "case12",
      "case13",
      "case14",
      "case15"
    ])

    expect(cells.get("case1")!.mainError).toEqual({
      kind: "died",
      rule: null,
      rfc: null,
      step: "s14",
      expected: "200 to INVITE",
      detail: "gated on early dialog \"f5\", which must be a dialog of its own"
    })
    expect(cells.get("case1")!.failedStep).toEqual("s14")
    expect(cells.get("case1")!.rfcGating).toEqual(0)

    expect(cells.get("case2")!.mainError).toEqual({
      kind: "rfc",
      rule: "rport-echo",
      rfc: "RFC 3581 §4",
      step: null,
      expected: null,
      detail: "200 response to a Via carrying rport echoes none — RFC 3581 §4 requires the server to set it"
    })
    expect(cells.get("case2")!.rfcGating).toEqual(1)

    expect(cells.get("case3")!.passed).toBe(true)
    expect(cells.get("case3")!.mainError).toBeNull()
    expect(cells.get("case3")!.document).toMatchObject({ case: { family: "transparent" } })

    expect(cells.get("case4")!.mainError).toEqual({
      kind: "refused",
      rule: "interpreter-refused",
      rfc: null,
      step: null,
      expected: null,
      detail: "replay exited 2 (refused) and left no bundle:"
    })
    expect(cells.get("case4")!.verdict).toEqual("absent")

    expect(cells.get("case5")!.skipped).toEqual("blocked:no-mrf")
    expect(cells.get("case5")!.mainError).toBeNull()

    // A verdict the contract refuses (a death with no gate) still names its step.
    expect(cells.get("case6")!.failedStep).toEqual("s3")
    expect(cells.get("case6")!.mainError).toEqual({
      kind: "died",
      rule: null,
      rfc: null,
      step: "s3",
      expected: null,
      detail: ""
    })

    // A clean verdict the reclassifier still refused names its confrontation records.
    expect(cells.get("case7")!.verdict).toEqual("ok")
    expect(cells.get("case7")!.mainError).toEqual({
      kind: "failure",
      rule: "reclassified",
      rfc: null,
      step: null,
      expected: null,
      detail: "confrontation: 2 unknown, 1 unlisted"
    })

    // A clean verdict failed by an SDP row reads that row, past the accepted ones.
    expect(cells.get("case8")!.mainError).toEqual({
      kind: "bad-body",
      rule: "bad-body",
      rfc: null,
      step: "s7",
      expected: null,
      detail: "body:sdp:m0:a=ptime:response:200:INVITE: a=ptime:20 → a=ptime:30"
    })
    expect(cells.get("case8")!.bodyFailing).toEqual(1)
    expect(cells.get("case1")!.bodyFailing).toEqual(0)

    // A death a named rule accepts (the shape record at s14), which the body
    // gate alone kept from the bless, reads bad-body at the earliest step of
    // the document, past a record that does not decode.
    expect(cells.get("case9")!.mainError).toEqual({
      kind: "bad-body",
      rule: "bad-body",
      rfc: null,
      step: "s3",
      expected: null,
      detail: "body:sdp:m0:a=direction:initial-invite: a=sendrecv → a=sendonly"
    })
    expect(cells.get("case9")!.bodyFailing).toEqual(2)

    // A clean verdict failed by body rows of other media types reads the
    // earliest by record order (no document), its first differing line named.
    expect(cells.get("case10")!.mainError).toEqual({
      kind: "bad-body",
      rule: "bad-body",
      rfc: null,
      step: "s33",
      expected: null,
      detail: "body:application/vnd.example.data:request:BYE:in-dialog: dwdHAIIDjQEB → (none)"
    })
    expect(cells.get("case10")!.bodyFailing).toEqual(2)

    // A passing cell whose only finding is a scripted party's violation the
    // document does not state: the source fault is information, named apart,
    // counted among no failing finding, and no main error.
    const source = {
      rule: "no-200-after-cancel",
      rfc: "RFC 3261 §9.2",
      detail: "uas1: Answered an INVITE it had already taken a CANCEL for with a 200 — " +
        "RFC 3261 §9.2 has the cancelled UAS answer 487"
    }
    expect(cells.get("case11")!.rfcGating).toEqual(0)
    expect(cells.get("case11")!.mainError).toBeNull()
    expect(cells.get("case11")!.sourceFault).toEqual(source)

    // A scripted party's violation the reference shows is information: the
    // cell passed and states no error.
    expect(cells.get("case12")!.passed).toBe(true)
    expect(cells.get("case12")!.rfcGating).toEqual(0)
    expect(cells.get("case12")!.mainError).toBeNull()

    // A death beside a source fault: the death is the main error, the source
    // fault stands beside it.
    expect(cells.get("case13")!.mainError).toEqual({
      kind: "died",
      rule: null,
      rfc: null,
      step: "s14",
      expected: "200 to INVITE",
      detail: "gated on early dialog \"f5\", which must be a dialog of its own",
      source
    })
    expect(cells.get("case13")!.sourceFault).toEqual(source)

    // A death the accepted shape record at its step answers, on a cell a
    // header record no rule accepts refused: that record is the main error.
    expect(cells.get("case14")!.mainError).toEqual({
      kind: "failure",
      rule: "unlisted",
      rfc: null,
      step: "s22",
      expected: null,
      detail: "an-unlisted-rule: header:allow:response:200:OPTIONS"
    })
    expect(cells.get("case14")!.sourceFault).toBeUndefined()

    // A bundle on disk that spells the source fault `harness_fault` reads the
    // same: the fault beside the death, and the case's source fault.
    expect(cells.get("case15")!.mainError?.source).toEqual(source)
    expect(cells.get("case15")!.sourceFault).toEqual(source)
    expect(cells.get("case15")!.rfcGating).toEqual(0)
  })

  it("honours the caller's ruling on the charges, and hands it the cell and the whole audit", async () => {
    const asked: Array<[string, ReadonlyArray<string>]> = []
    const reading = await readOf(RUN, {
      charges: (cellDir, findings) =>
        Effect.sync(() => {
          asked.push([path.basename(cellDir), findings.map((finding) => finding.rule)])
          return []
        })
    })
    const cells = byCase(reading)
    // Every charge inherited: the cell leads with where it died instead.
    expect(cells.get("case2")!.mainError?.kind).toEqual("died")
    expect(cells.get("case2")!.rfcGating).toEqual(1)
    // Asked only where the audit has a charge to rule on, with the peer findings as evidence.
    expect(asked).toEqual([
      [`${CAPTURE}-case2__pivot-replay__fake`, ["rport-echo", "cseq-in-dialog-order"]]
    ])
  })

  it("states each ran cell's lines, failing it where the rollup does, and none for a cell that did not run", async () => {
    const cells = byCase(await readOf(RUN))
    expect(cells.get("case1")!.verifications?.[0]).toMatchObject({
      name: "call",
      source: "interpreter",
      answer: "fail",
      detail: { step: "s14" }
    })
    expect(cells.get("case2")!.verifications?.find((line) => line.name === "rfc-audit")).toMatchObject({
      answer: "fail",
      message: expect.stringContaining("rport-echo")
    })
    expect(cells.get("case3")!.verifications).toEqual([
      { name: "call", source: "interpreter", answer: "pass" },
      { name: "rfc-audit", source: "rfc audit", answer: "pass" }
    ])
    expect(cells.get("case5")!.verifications).toBeUndefined()
    for (const [name, cell] of cells) {
      if (cell.verifications === undefined) continue
      expect([name, Verification.finalStatusOf(cell.verifications) === "pass"]).toEqual([name, cell.passed])
    }
  })

  it("states the lines a cell's lane answered, and a known bug as a waived line that fails nothing", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const cells = new Map(reading.cases.map((entry) => [entry.caseId.split("-").at(-1), entry]))
    const answered = cells.get("answered")!.verifications!
    expect(answered.map((line) => `${line.name} ${line.answer}`)).toEqual([
      "call pass",
      "cleanup pass",
      "call-records fail",
      "rfc-audit pass"
    ])
    expect(Verification.finalStatusOf(answered)).toBe("fail")
    const waived = cells.get("waived")!.verifications!
    expect(waived).toContainEqual(expect.objectContaining({
      name: "call",
      answer: "fail",
      waived: true,
      message: expect.stringContaining("known bug provisional-rewrite-not-applied")
    }))
    expect(Verification.finalStatusOf(waived)).toBe("pass")
  })

  it("states an audit that did not run as a not-checkable rfc-audit line, never a pass", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const unaudited = reading.cases.find((entry) => entry.caseId.endsWith("-unaudited"))!
    expect(unaudited.verifications).toEqual([
      { name: "call", source: "interpreter", answer: "pass" },
      {
        name: "rfc-audit",
        source: "rfc audit",
        answer: "not-checkable",
        message: "not audited: the lane's binder recorded no wire"
      }
    ])
    expect(Verification.finalStatusOf(unaudited.verifications!)).toBe("unverified")
  })

  it("leaves a cell whose lane line is still pending ok so far, the rollup not passing it", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const pending = reading.cases.find((entry) => entry.caseId.endsWith("-pending"))!
    expect(pending.passed).toBe(false)
    expect(pending.verifications!.map((line) => `${line.name} ${line.answer}`)).toEqual(["call pass", "cleanup pending"])
    expect(Verification.finalStatusOf(pending.verifications!)).toBe("ok so far")
  })

  it("states a failure the reclassifier restated as passed as a waived line it answers, naming its rules, and passes its own line", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const restated = reading.cases.find((entry) => entry.caseId.endsWith("-restated"))!
    expect(restated.passed).toBe(true)
    const lines = restated.verifications!
    expect(lines).toContainEqual({
      name: "call",
      source: "reclassifier",
      answer: "fail",
      message: "restated as passed (allow-trimmed — ok with accepted delta): check-failed: site=B1 180, field=header.Allow, " +
        "op=eq, expected=INVITE, ACK, observed=INVITE",
      detail: { step: "s7" },
      waived: true
    })
    expect(lines.filter((line) => line.answer === "fail" && Verification.decides(line))).toEqual([])
    expect(lines.at(-1)).toEqual({
      name: "reclassifier",
      source: "reclassifier",
      answer: "pass",
      message: "allow-trimmed — ok with accepted delta"
    })
    expect(Verification.finalStatusOf(lines)).toBe("pass")
  })

  it("states an RFC charge the reclassifier restated as passed as a waived audit line it answers", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const restated = reading.cases.find((entry) => entry.caseId.endsWith("-restated-audit"))!
    const lines = restated.verifications!
    expect(lines.map((line) => `${line.name} ${line.source} ${line.answer}${line.waived === true ? " waived" : ""}`)).toEqual([
      "call interpreter pass",
      "rfc-audit reclassifier fail waived",
      "reclassifier reclassifier pass"
    ])
    expect(lines[1]!.message).toContain("restated as passed (1 RFC charge(s) are a peer's own")
    expect(lines[1]!.message).toContain("sdp-origin-continuity")
    expect(Verification.finalStatusOf(lines)).toBe("pass")
  })

  it("leaves failing a lane line its batch's end failed after the reclassifier restated the cell as it landed", async () => {
    const reading = await readOf(path.join(import.meta.dirname, "fixtures", "run-lines"))
    const late = reading.cases.find((entry) => entry.caseId.endsWith("-restated-late"))!
    const lines = late.verifications!
    expect(lines.map((line) => `${line.name} ${line.source} ${line.answer}${line.waived === true ? " waived" : ""}`)).toEqual([
      "call reclassifier fail waived",
      "cleanup lane gate fail",
      "reclassifier reclassifier pass"
    ])
    expect(lines[1]).toEqual({
      name: "cleanup",
      source: "lane gate",
      answer: "fail",
      message: "a call left",
      detail: { seen: "a call left", side: "sut" }
    })
    expect(Verification.finalStatusOf(lines)).toBe("fail")
  })
})
