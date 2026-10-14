/**
 * The sliced corpus re-cut: one prep, worked a slice at a time.
 *
 * The cut phase itself is injected, so what is asserted is the LOOP — that the
 * captures reach it in slices of the size asked for, that a slice already in
 * `capture_cuts` is not cut again, and that the prep is finished exactly once.
 */
import { CaseIds, Cuts } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import type { CutReading } from "../../src/cut/cut-contract.js"
import { NOTHING_MIRRORED } from "../../src/cut/cut-rows.js"
import { sliced } from "../../src/cut/cut-slices.js"
import type { Options } from "../../src/options.js"
import { host as hostOf, optionsOf } from "./host.js"


let tree: string

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "cut-slices-test-"))
})
afterEach(() => {
  fs.rmSync(tree, { recursive: true, force: true })
})

const options = (): Options => optionsOf(tree)

const PREP = 200

interface Store {
  readonly layer: Layer.Layer<Cuts.Service | CaseIds.Service>
  readonly opened: Array<number>
  /** The job each opened prep was named under; undefined for a cut that is no job. */
  readonly openedUnder: Array<number | undefined>
  readonly finished: Array<number>
}

/** A store that hands out one prep and says which captures it already holds. */
const store = (done: ReadonlyArray<string>): Store => {
  const opened: Array<number> = []
  const openedUnder: Array<number | undefined> = []
  const finished: Array<number> = []
  const refuse = Effect.die(new Error("the slice loop asked this store the wrong question"))
  return {
    opened,
    openedUnder,
    finished,
    layer: Layer.merge(Layer.succeed(
      CaseIds.Service,
      CaseIds.Service.of({
        of: () => Effect.succeed(new Map()),
        freeze: () => refuse,
        markSeeded: () => refuse,
        unseeded: () => refuse,
        count: refuse
      })
    ), Layer.succeed(
      Cuts.Service,
      Cuts.Service.of({
        open: (_provenance, _fingerprint, jobId) => Effect.sync(() => {
          opened.push(PREP)
          openedUnder.push(jobId)
          return PREP
        }),
        exists: () => Effect.succeed(true),
        finish: (prepId) => Effect.sync(() => void finished.push(prepId)),
        record: () => refuse,
        standing: () => refuse,
        resolve: () => refuse,
        cases: () => refuse,
        standingPrep: refuse,
        latest: refuse,
        captures: () => Effect.succeed(done),
        caseSpans: () => Effect.succeed(new Map()),
        contentFrom: () => refuse,
        content: () => refuse,
        flowsEnvelope: () => refuse,
        flowsLegs: () => refuse,
        ledger: () => refuse
      })
    ))
  }
}

const nothing = (capture: string): CutReading => ({
  capture,
  prep: PREP,
  cutAt: "2026-08-28T00:00:00.000Z",
  flows: "flows.json",
  excluded: "excluded.json",
  cutFingerprint: "cut-code",
  provenance: [],
  cases: []
})

/** A census gate that lets every cut through. */
const passing = () => Effect.succeed("census.json")

/** A cut phase that runs no corpus: it records what it was asked and says it cut it. */
const recording = (asked: Array<ReadonlyArray<string>>, whileCutting?: () => void) =>
(captures: ReadonlyArray<string>, _options: Options, standing?: number) =>
  Effect.sync(() => {
    asked.push(captures)
    whileCutting?.()
    return {
      prep: standing ?? PREP,
      readings: new Map(captures.map((capture) => [capture, nothing(capture)])),
      mirrored: NOTHING_MIRRORED
    }
  })

const captures = (count: number): ReadonlyArray<string> =>
  Array.from({ length: count }, (_, at) => `capture_${at}.pcap.gz`)

describe("the sliced cut", () => {
  it("as a job, opens its prep under the job and tells each slice as a batch", async () => {
    const asked: Array<ReadonlyArray<string>> = []
    const batches: Array<[number, number]> = []
    const cuts = store([])
    const outcome = await Effect.runPromise(
      sliced({
        captures: captures(5),
        options: options(),
        size: 2,
        job: 7,
        onSlice: (at, of) => Effect.sync(() => void batches.push([at, of])),
        cut: recording(asked),
        gate: passing
      }).pipe(Effect.provide(Layer.merge(cuts.layer, hostOf(tree))))
    )
    expect(cuts.openedUnder).toEqual([7])
    expect(batches).toEqual([[1, 3], [2, 3], [3, 3]])
    expect(outcome.cut).toBe(5)
  })

  it("cuts the captures a slice at a time, under one prep", async () => {
    const asked: Array<ReadonlyArray<string>> = []
    const cuts = store([])
    const outcome = await Effect.runPromise(
      sliced({ captures: captures(5), options: options(), size: 2, cut: recording(asked), gate: passing }).pipe(
        Effect.provide(Layer.merge(cuts.layer, hostOf(tree)))
      )
    )

    expect(asked).toEqual([
      ["capture_0.pcap.gz", "capture_1.pcap.gz"],
      ["capture_2.pcap.gz", "capture_3.pcap.gz"],
      ["capture_4.pcap.gz"]
    ])
    expect(outcome).toMatchObject({ prep: PREP, cut: 5, resumed: 0, broke: 0 })
    expect(cuts.opened).toEqual([PREP])
    expect(cuts.finished).toEqual([PREP])
  })

  it("resumes from the captures the prep already cut", async () => {
    const asked: Array<ReadonlyArray<string>> = []
    const cuts = store(["capture_0.pcap.gz", "capture_1.pcap.gz", "capture_3.pcap.gz"])
    const outcome = await Effect.runPromise(
      sliced({
        captures: captures(5),
        options: options(),
        size: 2,
        resume: PREP,
        cut: recording(asked),
        gate: passing
      }).pipe(Effect.provide(Layer.merge(cuts.layer, hostOf(tree))))
    )

    // The first slice is whole in `capture_cuts`, so it is not cut at all.
    expect(asked).toEqual([["capture_2.pcap.gz"], ["capture_4.pcap.gz"]])
    expect(outcome).toMatchObject({ prep: PREP, cut: 2, resumed: 3 })
    // A resumed run continues the prep rather than opening one.
    expect(cuts.opened).toEqual([])
    expect(cuts.finished).toEqual([PREP])
  })
})
