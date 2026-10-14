/**
 * Reading a run bundle off disk, over a minimal one in `fixtures/`: every cell's
 * main error is filled from what the cell wrote, and the charges a caller rules
 * on are the ones the reading leads with.
 */
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
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
      "case13"
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

    // A cell failed on a scripted party's violation the document does not
    // state alone reads as the source's fault, naming the party; the SUT is
    // charged nothing.
    expect(cells.get("case11")!.passed).toBe(false)
    // Counted among the findings that fail it, so the row never reads 0 rfc.
    expect(cells.get("case11")!.rfcGating).toEqual(1)
    expect(cells.get("case11")!.mainError).toEqual({
      kind: "source-fault",
      rule: "no-200-after-cancel",
      rfc: "RFC 3261 §9.2",
      step: null,
      expected: null,
      detail: "uas1: Answered an INVITE it had already taken a CANCEL for with a 200 — " +
        "RFC 3261 §9.2 has the cancelled UAS answer 487"
    })

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
      source: {
        rule: "no-200-after-cancel",
        rfc: "RFC 3261 §9.2",
        detail: "uas1: Answered an INVITE it had already taken a CANCEL for with a 200 — " +
          "RFC 3261 §9.2 has the cancelled UAS answer 487"
      }
    })
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
})
