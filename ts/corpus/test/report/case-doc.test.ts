/**
 * One case's document by any cited
 * form — its directory on disk, its side rows, or its reference ladder —
 * read at the cut that names it, and a refusal saying why where none does.
 */
import { NodeServices } from "@effect/platform-node"
import { Cuts, Resolver, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { CaseLayout } from "../../src/deployment/index.js"
import * as CaseDoc from "../../src/report/case-doc.js"

const C = "trace.pcap.gz"
const ID = `${C}-case754`
const sessions = fs.mkdtempSync(path.join(os.tmpdir(), "corpus-case-doc-"))

afterAll(() => fs.rmSync(sessions, { recursive: true, force: true }))

const refuse = Effect.die("not in this test")

const resolution = (input: string, over: Partial<Resolver.Resolution>): Resolver.Resolution => ({
  input,
  form: Resolver.formOf(input, Resolver.DEFAULT_GRAMMAR),
  status: "resolved",
  calls: [{ capture: C, caseId: ID, callKey: "root@h", prepId: 12, kind: "case", reasons: [], standing: true }],
  total: 1,
  note: "prep12",
  ...over
})

const resolver = Layer.succeed(
  Resolver.Service,
  Resolver.Service.of({
    resolve: (input) => {
      if (input === ID || input === "run7/case754") return Effect.succeed(resolution(input, {}))
      if (input === `${C}-case1`) {
        return Effect.succeed(resolution(input, {
          calls: [{ capture: C, caseId: `${C}-case1`, callKey: "f@h", prepId: null, kind: "case", reasons: [], standing: false }],
          note: "a frozen id: no reading the store holds names it"
        }))
      }
      return Effect.succeed(resolution(input, { status: "not-found", calls: [], total: 0, note: `no cut names ${input}` }))
    }
  })
)

const cuts = Layer.succeed(
  Cuts.Service,
  Cuts.Service.of({
    open: () => refuse,
    finish: () => refuse,
    exists: () => refuse,
    record: () => refuse,
    standing: (capture, prep) =>
      Effect.succeed(
        capture === C && prep === 12
          ? {
            prepId: 12,
            capture: C,
            flowsHash: null,
            broke: null,
            cutAt: "2026-10-12T00:00:00Z",
            cases: [{ caseId: ID, callKey: "root@h", caseHash: "hash754", egressEndpoint: null }]
          }
          : null
      ),
    resolve: () => refuse,
    cases: () => refuse,
    standingPrep: refuse,
    latest: refuse,
    captures: () => refuse,
    caseSpans: () => refuse,
    contentFrom: () => refuse,
    content: (hash) =>
      Effect.succeed(
        hash === "hash754"
          ? {
            document: { case: { id: ID }, pivot_version: 3, flow: [] },
            assets: [],
            ledger: [{ number: "1001", document: { decision: "route" } }]
          }
          : null
      ),
    flowsEnvelope: () => refuse,
    flowsLegs: () => refuse,
    ledger: () => refuse
  })
)

const run = (input: CaseDoc.Input) =>
  Effect.runPromise(CaseDoc.read(sessions, input).pipe(Effect.provide(Layer.mergeAll(resolver, cuts, StoreBinding.layer, CaseLayout.layer, NodeServices.layer))))

describe("CaseDoc.read", () => {
  it("prints the case directory, its document and its side rows, materialised, off <capture> <caseN>", async () => {
    const got = await run({ capture: C, id: "case754", print: "path" })
    expect(got._tag).toBe("lines")
    const lines = got._tag === "lines" ? got.lines : []
    const document = lines.find((line) => line.startsWith("document"))!.split(/\s+/)[1]!
    expect(fs.existsSync(document)).toBe(true)
    expect(path.basename(document)).toBe(`${ID}.v3.json`)
    expect(lines[0]).toContain(`${ID}  prep12`)
  })

  it("prints the side rows the case carries", async () => {
    const got = await run({ id: "run7/case754", print: "ledger" })
    expect(got._tag === "lines" ? got.lines.slice(1) : []).toEqual([
      '1001  {"decision":"route"}'
    ])
  })

  it("refuses the reference ladder where the cut stored no flows document", async () => {
    const got = await run({ id: ID, print: "flow" })
    expect(got._tag === "refused" ? got.why : "").toContain("no flows document")
  })

  it("refuses an id only the frozen ids hold: no reading has its document", async () => {
    const got = await run({ id: `${C}-case1`, print: "path" })
    expect(got._tag === "refused" ? got.why : "").toContain("no reading the store holds names it")
  })

  it("refuses an id nothing names, with the resolver's reason", async () => {
    const got = await run({ capture: C, id: "case9", print: "path" })
    expect(got._tag === "refused" ? got.why : "").toContain(`no cut names ${C}-case9`)
  })
})
