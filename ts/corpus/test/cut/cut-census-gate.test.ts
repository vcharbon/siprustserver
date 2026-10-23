/**
 * The census gate of a cut: one coverage line for the whole cut, and under
 * `--strict-census` a capture the census run did not sweep refuses the cut
 * before any prep opens or anything is enriched. An empty run is refused by name.
 */
import { CaseIds, Cuts } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { CensusRefused, censusGate, phase, seedRefusals } from "../../src/cut/cut.js"
import { Launcher } from "../../src/deployment/index.js"
import type { Options } from "../../src/options.js"
import { host as hostOf, optionsOf } from "./host.js"

let tree: string

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "cut-census-gate-"))
  fs.mkdirSync(path.join(tree, "census"))
})
afterEach(() => {
  vi.restoreAllMocks()
  fs.rmSync(tree, { recursive: true, force: true })
})

/** A dated census run over `captures`, laid out as the sweep writes it: the list before the hits. */
const census = (captures: ReadonlyArray<string>): string => {
  const file = path.join(tree, "census", "2026-10-12.census.json")
  fs.writeFileSync(file, `${JSON.stringify({ documents: captures.length, captures, groups: 0, hits: [] }, null, 2)}\n`)
  return file
}

const options = (strictCensus: boolean): Options => ({ ...optionsOf(tree, strictCensus), batchSize: 1 })

/** A store that answers nothing: a cut that asks it anything has gone past the gate. */
const untouched = (): Layer.Layer<Cuts.Service | CaseIds.Service> => {
  const asked = Effect.die(new Error("the cut asked the store past a refusing gate"))
  const ids = Layer.succeed(CaseIds.Service, CaseIds.Service.of({
    of: () => asked,
    freeze: () => asked,
    markSeeded: () => asked,
    unseeded: () => asked,
    count: asked
  }))
  return Layer.merge(ids, Layer.succeed(
    Cuts.Service,
    Cuts.Service.of({
      open: () => asked,
      exists: () => asked,
      finish: () => asked,
      record: () => asked,
      standing: () => asked,
      resolve: () => asked,
      cases: () => asked,
      standingPrep: asked,
      latest: asked,
      captures: () => asked,
      caseSpans: () => asked,
      contentFrom: () => asked,
      content: () => asked,
      flowsEnvelope: () => asked,
      flowsLegs: () => asked,
      ledger: () => asked
    })
  ))
}

const said = (): Array<string> => {
  const lines: Array<string> = []
  vi.spyOn(console, "log").mockImplementation((line: unknown) => void lines.push(String(line)))
  return lines
}

describe("the census gate of a cut", () => {
  it("refuses a strict cut in batches of one over a covered and an uncovered capture before any prep opens", async () => {
    census(["covered.pcap.gz"])
    const lines = said()
    const refused = await Effect.runPromise(
      phase(["covered.pcap.gz", "uncovered.pcap.gz"], options(true)).pipe(Effect.provide(Layer.merge(untouched(), hostOf(tree))), Effect.flip)
    )
    expect(refused).toBeInstanceOf(CensusRefused)
    if (!(refused instanceof CensusRefused)) return
    expect(refused.reason).toContain("does not cover 1 capture(s)")
    expect(lines.filter((line) => line.includes("census covers"))).toEqual([
      "[cut] census covers 1/2 captures; uncovered: uncovered.pcap.gz (census 2026-10-12.census.json)"
    ])
  })

  it("lets a cut without --strict-census through, the coverage line said once", async () => {
    const file = census(["covered.pcap.gz"])
    const lines = said()
    const run = await Effect.runPromise(
      censusGate(["covered.pcap.gz", "uncovered.pcap.gz"], options(false)).pipe(Effect.provide(hostOf(tree)))
    )
    expect(run).toBe(file)
    expect(lines).toEqual([
      "[cut] census covers 1/2 captures; uncovered: uncovered.pcap.gz (census 2026-10-12.census.json)"
    ])
  })

  it("lets a strict cut through where every capture is covered", async () => {
    census(["a.pcap.gz", "b.pcap.gz"])
    said()
    const run = await Effect.runPromiseExit(
      censusGate(["a.pcap.gz", "b.pcap.gz"], options(true)).pipe(Effect.provide(hostOf(tree)))
    )
    expect(Exit.isSuccess(run)).toBe(true)
  })

  it("refuses a capture read before its ids were frozen and never seeded, naming the deployment's seed command", async () => {
    const ids = Layer.succeed(CaseIds.Service, CaseIds.Service.of({
      of: () => Effect.die(new Error("not asked")),
      freeze: () => Effect.die(new Error("not asked")),
      markSeeded: () => Effect.die(new Error("not asked")),
      unseeded: (captures) => Effect.succeed(captures.filter((capture) => capture === "old.pcap.gz")),
      count: Effect.die(new Error("not asked"))
    }))
    const launcher = Launcher.layerWith({
      entry: () => Effect.die(new Error("not asked")),
      cwd: tree,
      hooksCwd: tree,
      hooks: Effect.succeed([]),
      seedIds: "`seed-the-ids`"
    })
    const refused = await Effect.runPromise(
      seedRefusals(["old.pcap.gz", "new.pcap.gz"]).pipe(Effect.provide(Layer.merge(ids, launcher)))
    )
    expect([...refused.keys()]).toEqual(["old.pcap.gz"])
    expect(refused.get("old.pcap.gz")).toContain("run `seed-the-ids` first")
  })

  it("refuses an empty census run by name, strict or not", async () => {
    fs.writeFileSync(path.join(tree, "census", "2026-10-12.census.json"), "")
    const refused = await Effect.runPromise(
      censusGate(["a.pcap.gz"], options(false)).pipe(Effect.provide(hostOf(tree)), Effect.flip)
    )
    expect(refused.reason).toContain("is empty")
  })
})
