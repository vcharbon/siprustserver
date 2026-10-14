/**
 * What the cut mirrors into the store, and what a failed mirror costs.
 *
 * Two claims, and both are about the boundary rather than about SQL: the rows a
 * capture becomes are exactly what the generator left in the cut's scratch, and a
 * capture the store refuses costs the captures beside it nothing — every one of
 * them is still offered, and the scratch is untouched by the failure.
 */
import { NodeServices } from "@effect/platform-node"
import { Cuts, type Model } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { ConnectionError, SqlError } from "effect/unstable/sql/SqlError"
import * as crypto from "node:crypto"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { Buffer } from "node:buffer"
import type { CutReading } from "../../src/cut/cut-contract.js"
import { CaseLayout } from "../../src/deployment/index.js"
import * as CutRows from "../../src/cut/cut-rows.js"

let tree: string

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "cut-rows-test-"))
})
afterEach(() => {
  fs.rmSync(tree, { recursive: true, force: true })
})

const PREP = 200

/** Side rows under `rows/`, each stating its call key as `anchor`. */
const layout = CaseLayout.layerWith({
  rowsDir: "rows",
  rowFile: (number) => `${number}.json`,
  callKeyOf: (row) => {
    const anchor = (row.document as { anchor?: unknown }).anchor
    return typeof anchor === "string" && anchor !== "" ? anchor : undefined
  },
  interpreterVariable: undefined
})

/** A flows document of two legs, laid out as the extractor's enrichment lays one out. */
const flowsDocument = (capture: string): string =>
  `${JSON.stringify({ schema: 5, capture, legs: [{ call_id: "a" }, { call_id: "b" }] }, null, 2)}\n`

/** A capture's cut, exactly as the generator leaves it in the cut's scratch. */
const cutCapture = (
  capture: string,
  options?: { readonly broke?: string; readonly cases?: number; readonly refused?: ReadonlyArray<string> }
): CutReading => {
  const scratch = path.join(tree, capture)
  const flows = path.join(scratch, "src", `${capture}.flows.json`)
  fs.mkdirSync(path.dirname(flows), { recursive: true })
  fs.writeFileSync(flows, flowsDocument(capture))
  const excluded = path.join(scratch, "out", "excluded.json")
  if (options?.refused !== undefined) {
    fs.mkdirSync(path.dirname(excluded), { recursive: true })
    fs.writeFileSync(
      excluded,
      `${JSON.stringify(options.refused.map((reason, at) => ({
        caseId: `${capture}-cut${at + 1}`,
        reason,
        legs: [1],
        callIds: [`call-${at + 1}@x`],
        evidence: [{ leg: 1, hop: 0, detail: `leg 1 ${reason}` }],
        line: `${capture}: case '${capture}-cut${at + 1}' EXCLUDED ${reason} — leg 1 ${reason}`
      })), null, 2)}\n`
    )
  }
  const cases = []
  for (let at = 1; at <= (options?.cases ?? 1); at++) {
    const id = `${capture}-case${at}`
    const caseDir = path.join(scratch, "out", "cases", id)
    fs.mkdirSync(path.join(caseDir, "resources"), { recursive: true })
    fs.mkdirSync(path.join(caseDir, "rows"), { recursive: true })
    fs.writeFileSync(
      path.join(caseDir, `${id}.v3.json`),
      `${JSON.stringify({ case: { id }, actors: [{ kind: "uas", endpoint: "ep2" }] }, null, 2)}\n`
    )
    fs.writeFileSync(path.join(caseDir, "resources", "uac1_0_0.sdp"), `v=0\r\no=${id}\r\n`)
    fs.writeFileSync(
      path.join(caseDir, "rows", `100${at}.json`),
      `${JSON.stringify({ number: `100${at}`, dialled: "route", anchor: `SDcall${at}-${capture}` })}\n`
    )
    cases.push({ id, dir: caseDir, egressEndpoint: "ep2" })
  }
  const reading: CutReading = {
    capture,
    prep: PREP,
    cutAt: "2026-08-28T00:00:00.000Z",
    flows,
    provenance: [],
    cutFingerprint: "cut-code",
    ...(options?.broke === undefined ? {} : { broke: options.broke }),
    cases: options?.broke === undefined ? cases : [],
    excluded
  }
  return reading
}

/** Every file in the tree, by path, with its content hash: what a mirror must not move. */
const snapshot = (dir: string, base = dir): ReadonlyMap<string, string> => {
  const seen = new Map<string, string>()
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const child = path.join(dir, entry.name)
    if (entry.isDirectory()) for (const [at, hash] of snapshot(child, base)) seen.set(at, hash)
    else if (entry.isSymbolicLink()) seen.set(path.relative(base, child), `-> ${fs.readlinkSync(child)}`)
    else seen.set(path.relative(base, child), crypto.createHash("sha256").update(fs.readFileSync(child)).digest("hex"))
  }
  return seen
}

/** A store that answers what it was told to, and remembers what it was handed. */
const store = (
  record: (request: Model.CaptureCutRequest) => Effect.Effect<Model.Recorded, SqlError>
): { readonly layer: Layer.Layer<Cuts.Service>; readonly seen: Array<Model.CaptureCutRequest> } => {
  const seen: Array<Model.CaptureCutRequest> = []
  const refuse = Effect.die(new Error("this test's store answers only `record`"))
  return {
    seen,
    layer: Layer.succeed(
      Cuts.Service,
      Cuts.Service.of({
        open: () => refuse,
        exists: () => refuse,
        finish: () => refuse,
        record: (request) => {
          seen.push(request)
          return record(request)
        },
        standing: () => refuse,
        resolve: () => refuse,
        cases: () => refuse,
        standingPrep: refuse,
        latest: refuse,
        contentFrom: () => refuse,
        captures: () => refuse,
        caseSpans: () => refuse,
        content: () => refuse,
        flowsEnvelope: () => refuse,
        flowsLegs: () => refuse,
        ledger: () => refuse
      })
    )
  }
}

const stored = (request: Model.CaptureCutRequest): Model.Recorded => ({
  cases: request.cases.map((entry) => ({ caseId: entry.caseId, caseHash: "hash", fresh: false }))
})

const mirror = (
  readings: ReadonlyMap<string, CutReading>,
  layer: Layer.Layer<Cuts.Service>
): Promise<CutRows.Mirrored> =>
  Effect.runPromise(CutRows.mirror(PREP, readings).pipe(Effect.provide(Layer.mergeAll(layer, layout, NodeServices.layer))))

/** One reading as a store request, read under the test's side-row layout. */
const requestOf = (reading: CutReading) =>
  Effect.runPromise(CutRows.requestOf(PREP, reading).pipe(Effect.provide(Layer.merge(layout, NodeServices.layer))))

describe("the rows a cut becomes", () => {
  it("carries what the generator wrote: the flows, the document, the resources and the side rows", async () => {
    const prepared = cutCapture("capture_rows.pcap.gz")
    const request = await requestOf(prepared)

    expect(request.prepId).toEqual(PREP)
    expect(request.capture).toEqual("capture_rows.pcap.gz")
    // The document reaches the store as its pieces, which are the file again.
    const flows = [...request.flows!()]
    expect(flows.map((part) => part.kind)).toEqual(["head", "leg", "leg", "tail"])
    expect(new TextDecoder().decode(Buffer.concat(flows.map((part) => part.bytes))))
      .toEqual(flowsDocument("capture_rows.pcap.gz"))
    expect(request.broke).toBeNull()
    expect(request.cases).toHaveLength(1)

    const entry = request.cases[0]!
    expect(entry.caseId).toEqual("capture_rows.pcap.gz-case1")
    expect(entry.egressEndpoint).toEqual("ep2")
    // Parsed, not bytes: the hash must be the one a replay's document takes.
    expect(entry.document).toEqual({
      case: { id: "capture_rows.pcap.gz-case1" },
      actors: [{ kind: "uas", endpoint: "ep2" }]
    })
    expect(entry.assets.map((asset) => asset.path)).toEqual(["resources/uac1_0_0.sdp"])
    expect(new TextDecoder().decode(entry.assets[0]!.bytes)).toContain("o=capture_rows")
    expect(entry.ledger.map((row) => row.number)).toEqual(["1001"])
    expect(entry.ledger[0]!.document).toEqual({
      number: "1001",
      dialled: "route",
      anchor: "SDcall1-capture_rows.pcap.gz"
    })
    // The call key is the side row's anchor: the capture's own name for the call.
    expect(entry.callKey).toEqual("SDcall1-capture_rows.pcap.gz")
    // `scenario.json` is not content: the materialiser makes its own symlink.
    expect(entry.assets.some((asset) => asset.path.includes("scenario.json"))).toBe(false)
  })

  it("gives two cases of one capture the keys their side rows state, distinct", async () => {
    const request = await requestOf(cutCapture("capture_two.pcap.gz", { cases: 2 }))
    expect(request.cases.map((entry) => entry.callKey)).toEqual([
      "SDcall1-capture_two.pcap.gz",
      "SDcall2-capture_two.pcap.gz"
    ])
  })

  it("refuses to make a key up for a case whose side rows state none, or two", async () => {
    const none = cutCapture("capture_keyless.pcap.gz")
    const ledger = path.join(none.cases[0]!.dir, "rows")
    fs.writeFileSync(path.join(ledger, "1001.json"), `${JSON.stringify({ number: "1001" })}\n`)
    await expect(requestOf(none)).rejects.toThrow(/states 0 call key/)

    const two = cutCapture("capture_twokeys.pcap.gz")
    fs.writeFileSync(
      path.join(two.cases[0]!.dir, "rows", "1002.json"),
      `${JSON.stringify({ number: "1002", anchor: "SDother" })}\n`
    )
    await expect(requestOf(two)).rejects.toThrow(/states 2 call key/)
  })

  it("keeps a broken cut apart from one that refused every case", async () => {
    const broke = await requestOf(cutCapture("capture_broke.pcap.gz", { broke: "[prep200] enrich failed" }))
    expect(broke.broke).toEqual("[prep200] enrich failed")
    expect(broke.cases).toEqual([])

    const refused = await requestOf(cutCapture("capture_refused.pcap.gz", { cases: 0 }))
    expect(refused.broke).toBeNull()
    expect(refused.cases).toEqual([])
    // No `excluded.json` is no refusals, not a failure to read one.
    expect(refused.refusals).toEqual([])
  })

  it("carries what the cut refused, off the excluded.json it left", async () => {
    const capture = "capture_partly_refused.pcap.gz"
    const request = await requestOf(

      cutCapture(capture, { cases: 1, refused: ["egress-answer-not-captured", "source-call-incomplete"] })
    )
    expect(request.cases).toHaveLength(1)
    expect(request.refusals).toEqual([
      {
        caseId: `${capture}-cut1`,
        reasons: ["egress-answer-not-captured"],
        callIds: ["call-1@x"],
        detail: `${capture}: case '${capture}-cut1' EXCLUDED egress-answer-not-captured — leg 1 egress-answer-not-captured`
      },
      {
        caseId: `${capture}-cut2`,
        reasons: ["source-call-incomplete"],
        callIds: ["call-2@x"],
        detail: `${capture}: case '${capture}-cut2' EXCLUDED source-call-incomplete — leg 1 source-call-incomplete`
      }
    ])
  })

  it("carries where each call starts and the key a reused id replaces, off the names file the cut left", async () => {
    const capture = "capture_named.pcap.gz"
    const prepared = cutCapture(capture, { cases: 1, refused: ["source-call-incomplete"] })
    const names = path.join(path.dirname(prepared.excluded), "names.json")
    const start = { at_us: 1_789_646_119_005_979, src: "10.0.0.9:5060", dst: "10.0.0.1:5060" }
    fs.writeFileSync(names, JSON.stringify({
      [`${capture}-case1`]: { start, replaces: "SDcall1#ingress0" },
      [`${capture}-cut1`]: { replaces: "call-1@x#ingress1" }
    }))
    const request = await requestOf({ ...prepared, names })
    expect(request.cases[0]).toMatchObject({ start, replaces: "SDcall1#ingress0" })
    expect(request.refusals[0]).toMatchObject({ caseId: `${capture}-cut1`, replaces: "call-1@x#ingress1" })
    // A reading with no names file names nothing more.
    expect((await requestOf(prepared)).cases[0]!.start).toBeUndefined()
  })

  it("carries a call refused under two verdicts as one refusal with both reasons", async () => {
    const capture = "capture_twice_refused.pcap.gz"
    const scratch = path.join(tree, capture, "out")
    fs.mkdirSync(scratch, { recursive: true })
    fs.writeFileSync(
      path.join(scratch, "excluded.json"),
      JSON.stringify([
        { caseId: `${capture}-auto`, reason: "egress-answer-not-captured", callIds: ["a@x"], line: "one" },
        { caseId: `${capture}-auto`, reason: "source-call-incomplete", callIds: ["a@x", "b@x"], line: "two" }
      ])
    )
    const reading: CutReading = { ...cutCapture(capture, { cases: 0 }), excluded: path.join(scratch, "excluded.json") }
    expect((await requestOf(reading)).refusals).toEqual([
      {
        caseId: `${capture}-auto`,
        reasons: ["egress-answer-not-captured", "source-call-incomplete"],
        callIds: ["a@x", "b@x"],
        detail: "one; two"
      }
    ])
  })
})

describe("a store that refuses the rows", () => {
  it("counts it, leaves the scratch alone, and mirrors the capture behind it", async () => {
    const first = cutCapture("capture_refused_rows.pcap.gz")
    const second = cutCapture("capture_second.pcap.gz")
    const before = snapshot(tree)

    const failing = store((request) =>
      request.capture === first.capture
        ? Effect.fail(
          new SqlError({
            reason: new ConnectionError({ cause: new Error("the server is away"), message: "no connection" })
          })
        )
        : Effect.succeed(stored(request))
    )
    const mirrored = await mirror(
      new Map([[first.capture, first], [second.capture, second]]),
      failing.layer
    )

    expect(mirrored.failed).toEqual(1)
    expect(mirrored.recorded).toEqual(1)
    // The capture behind the failure was still offered to the store.
    expect(failing.seen.map((request) => request.capture)).toEqual([first.capture, second.capture])
    expect(snapshot(tree)).toEqual(before)
  })

  it("counts a store that dies as loudly as one that fails", async () => {
    const prepared = cutCapture("capture_died.pcap.gz")
    const dying = store(() => Effect.die(new Error("the pool is closed")))
    const mirrored = await mirror(new Map([[prepared.capture, prepared]]), dying.layer)
    expect(mirrored).toEqual({ recorded: 0, failed: 1, cases: 0, fresh: 0 })
  })

  it("counts what the store had already seen apart from what is new to it", async () => {
    const prepared = cutCapture("capture_known.pcap.gz", { cases: 3 })
    const agreeing = store((request) =>
      Effect.succeed({
        cases: request.cases.map((entry, at) => ({
          caseId: entry.caseId,
          caseHash: `hash${at}`,
          fresh: at === 2
        }))
      })
    )
    const mirrored = await mirror(new Map([[prepared.capture, prepared]]), agreeing.layer)
    expect(mirrored).toEqual({ recorded: 1, failed: 0, cases: 3, fresh: 1 })
  })
})
