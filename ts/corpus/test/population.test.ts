/**
 * The population predicates, as claims about which captures survive them.
 *
 * The sampler's claims are the load-bearing ones: it must draw the SAME set
 * twice, or a re-run compares one population against a different one and the
 * before/after says nothing.
 */
import { NodeServices } from "@effect/platform-node"
import { Cuts, Model, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import type { Paths } from "../src/paths.js"
import * as Population from "../src/population.js"

import { type Selection, stated } from "../src/universe.js"

const refuse = Effect.die("not asked for")

/** A cut store whose standing cut holds these captures, each case with its span or none. */
const caseSpansOf = (spans: Record<string, Record<string, number | null>>) =>
  Layer.succeed(Cuts.Service)(Cuts.Service.of({
    open: () => refuse,
    exists: () => refuse,
    finish: () => refuse,
    record: () => refuse,
    standing: () => refuse,
    resolve: () => refuse,
    standingPrep: refuse,
    captures: () => refuse,
    caseSpans: () =>
      Effect.succeed(
        new Map(Object.entries(spans).map(([capture, cases]) => [capture, new Map(Object.entries(cases))]))
      ),
    contentFrom: () => refuse,
    content: () => refuse,
    ledger: () => refuse
  } as unknown as Cuts.Interface))

const standing = (capture: string, outcome: Partial<Model.Standing>): Model.Standing => ({
  capture,
  lane: "paused",
  runId: 1,
  status: "done",
  prepId: 1,
  bundlePath: "",
  finishedAt: null,
  cases: 1,
  failed: 0,
  skipped: 0,
  caseIds: [],
  failedSteps: [],
  skipReasons: [],
  caseResults: [],
  rfcGating: 0,
  deltas: Model.NO_DELTAS,
  lastOkRunId: null,
  lastOkBundlePath: null,
  caseRuns: [],
  ...outcome
})

const runsOf = (
  standings: ReadonlyArray<Model.Standing>,
  lanes: ReadonlyArray<string> = ["paused"]
) =>
  Layer.succeed(Runs.Service)(Runs.Service.of({
    standings: () => Effect.succeed(standings),
    lanes: Effect.succeed(lanes.map((lane) => ({ lane, runs: 1, captures: 1 }))),
    ranOn: () => Effect.succeed([])
  } as unknown as Runs.Interface))

const selection = (over: Partial<Selection>): Selection => ({
  universe: "all",
  positional: [],
  onlyNew: false,
  maxSpanSeconds: undefined,
  greenOnLane: undefined,
  sampleSize: undefined,
  limit: undefined,
  cases: [],
  ...over
})

const captureNames = (count: number): ReadonlyArray<string> =>
  Array.from({ length: count }, (_, index) => `capture_${String(index).padStart(4, "0")}.pcap.gz`)

describe("Population.withinSpan", () => {
  const list = [
    "short.pcap.gz",
    "exact.pcap.gz",
    "mixed.pcap.gz",
    "long.pcap.gz",
    "untimed.pcap.gz",
    "half-timed.pcap.gz"
  ]
  const layer = caseSpansOf({
    "short.pcap.gz": { "short.pcap.gz-auto": 5_000 },
    "exact.pcap.gz": { "exact.pcap.gz-case1": 60_000, "exact.pcap.gz-case2": 1_000 },
    "mixed.pcap.gz": {
      "mixed.pcap.gz-case1": 5_000,
      "mixed.pcap.gz-case2": 61_000,
      "mixed.pcap.gz-case3": 60_000
    },
    "long.pcap.gz": { "long.pcap.gz-case1": 61_000, "long.pcap.gz-case2": 3_600_000 },
    "untimed.pcap.gz": { "untimed.pcap.gz-auto": null },
    "half-timed.pcap.gz": { "half-timed.pcap.gz-case1": 2_000, "half-timed.pcap.gz-case2": null }
  })
  const within = (seconds: number) =>
    Effect.runPromise(Population.withinSpan(list, seconds, undefined).pipe(Effect.provide(layer)))

  it("replays a capture whose every call is at or under the bound whole", () =>
    within(60).then((kept) => {
      expect(kept.filter((work) => work.set.kind === "full").map((work) => work.capture))
        .toEqual(["short.pcap.gz", "exact.pcap.gz"])
    }))

  it("names the calls under the bound of a capture that has calls over it", () =>
    within(60).then((kept) => {
      expect(kept.find((work) => work.capture === "mixed.pcap.gz")).toEqual({
        capture: "mixed.pcap.gz",
        set: { kind: "cases", cases: ["mixed.pcap.gz-case1", "mixed.pcap.gz-case3"] }
      })
    }))

  it("drops a capture whose every call is over the bound", () =>
    within(60).then((kept) => {
      expect(kept.map((work) => work.capture)).not.toContain("long.pcap.gz")
    }))

  it("drops a capture the cut store states no span for", () =>
    within(3600).then((kept) => {
      expect(kept.map((work) => work.capture)).not.toContain("untimed.pcap.gz")
    }))

  it("leaves a call with no stated span out, so its capture is a cases set of the timed ones", () =>
    within(3600).then((kept) => {
      expect(kept.find((work) => work.capture === "half-timed.pcap.gz")).toEqual({
        capture: "half-timed.pcap.gz",
        set: { kind: "cases", cases: ["half-timed.pcap.gz-case1"] }
      })
    }))

  it("answers in the order it was asked", () =>
    within(60).then((kept) => {
      expect(kept.map((work) => work.capture)).toEqual([
        "short.pcap.gz",
        "exact.pcap.gz",
        "mixed.pcap.gz",
        "half-timed.pcap.gz"
      ])
    }))
})

describe("Population.greenOn", () => {
  it("keeps only the captures whose latest run on the lane was green", () =>
    Effect.runPromise(
      Population.greenOn(
        ["green.pcap.gz", "red.pcap.gz", "skipped.pcap.gz", "crashed.pcap.gz", "never.pcap.gz"],
        "paused"
      ).pipe(
        Effect.provide(runsOf([
          standing("green.pcap.gz", {}),
          standing("red.pcap.gz", { failed: 1 }),
          // Every case blocked on the lane: the lane was never asked, so not green.
          standing("skipped.pcap.gz", { skipped: 1 }),
          standing("crashed.pcap.gz", { status: "error" })
        ]))
      )
    ).then((kept) => {
      expect(kept).toEqual(["green.pcap.gz"])
    }))
})

describe("Population.sample", () => {
  const corpus = captureNames(500)

  it("draws the same captures every time", () => {
    expect(Population.sample(corpus, 50)).toEqual(Population.sample(corpus, 50))
  })

  it("draws from across the corpus rather than off one end", () => {
    const drawn = Population.sample(corpus, 50)
    expect(drawn).toHaveLength(50)
    // A truncation of the sorted list would put every draw in the first tenth.
    expect(drawn.filter((capture) => corpus.indexOf(capture) >= 250).length).toBeGreaterThan(10)
  })

  it("answers in capture order, so the work list runs in the order every other selection runs in", () => {
    const drawn = Population.sample(corpus, 50)
    expect([...drawn].sort()).toEqual(drawn)
  })

  it("holds a draw steady as the population around it shrinks", () => {
    const drawn = Population.sample(corpus, 50)
    const survivors = corpus.filter((capture) => drawn.includes(capture) || capture.endsWith("9.pcap.gz"))
    // Every capture the first draw took is still there, so the second takes them again.
    expect(Population.sample(survivors, 50)).toEqual(drawn)
  })

  it("answers the whole population when it is smaller than the draw", () => {
    expect(Population.sample(corpus.slice(0, 10), 50)).toEqual(corpus.slice(0, 10))
  })
})

/** A corpus of exactly these capture directories, and the paths naming it. */
const corpusOf = (names: ReadonlyArray<string>): Paths => {
  const corpus = fs.mkdtempSync(path.join(os.tmpdir(), "population-corpus-"))
  for (const name of names) fs.mkdirSync(path.join(corpus, name))
  corpora.push(corpus)
  return pathsAt(corpus)
}
const corpora: Array<string> = []

/** The roots of a fixture corpus: the rest stated beside it, never read. */
const pathsAt = (corpus: string): Paths => ({
  corpus,
  sessions: path.join(corpus, "_sessions"),
  census: path.join(corpus, "_census"),
  registry: path.join(corpus, "_registry.json"),
  repoRoot: corpus
})
afterAll(() => {
  for (const dir of corpora) fs.rmSync(dir, { force: true, recursive: true })
})

describe("Population.refusal", () => {
  const run = (over: Partial<Selection>, lanes?: ReadonlyArray<string>, paths: Paths = corpusOf([])) =>
    Effect.runPromise(
      Population.refusal(paths, selection(over)).pipe(Effect.provide(Layer.merge(runsOf([], lanes), NodeServices.layer)))
    )

  it("passes a selection that states no predicate", () => run({}).then((why) => expect(why).toBeNull()))

  it("refuses a bound that is not a positive number", async () => {
    expect(await run({ maxSpanSeconds: Number.NaN })).toContain("--max-span")
    expect(await run({ sampleSize: 0 })).toContain("--sample")
    expect(await run({ limit: -1 })).toContain("--limit")
  })

  // A case set is one capture's: `--case` over two captures, or over a derived
  // universe, would name a case of which capture?
  describe("a case set", () => {
    const held = ["capture_0011.pcap.gz", "capture_0012.pcap.gz"]
    const corpus = () => corpusOf(held)

    it("passes one named capture with its cases", async () => {
      expect(
        await run({ positional: [held[0]!], universe: "named", cases: [`${held[0]}-case1`] }, undefined, corpus())
      ).toBeNull()
    })

    it("refuses two positional captures, in one line", async () => {
      const why = await run({ positional: held, universe: "named", cases: [`${held[0]}-case1`] }, undefined, corpus())
      expect(why).toContain("--case")
      expect(why).not.toContain("\n")
    })

    it("refuses a derived universe", async () => {
      for (const universe of ["all", "standing", "known"] as const) {
        const why = await run({ universe, cases: ["capture_0011.pcap.gz-case1"] }, undefined, corpus())
        expect(why).toContain("--case")
        expect(why).toContain(`--${universe}`)
      }
    })

    // Two answers to "which calls": the named ones, or the ones under the bound.
    it("refuses a span bound beside it", async () => {
      const why = await run(
        { positional: [held[0]!], universe: "named", cases: [`${held[0]}-case1`], maxSpanSeconds: 120 },
        undefined,
        corpus()
      )
      expect(why).toContain("--case")
      expect(why).toContain("--max-span")
    })
  })

  it("refuses a --green-on lane the run record has never seen", async () => {
    expect(await run({ greenOnLane: "paused" })).toBeNull()
    // Silently selecting nothing would read as a corpus with nothing to do.
    expect(await run({ greenOnLane: "pause" })).toContain("names no lane")
  })

  // A named capture the corpus does not hold is refused before any run row
  // opens: the row would sit in the `--all` universe, erroring on every sweep.
  describe("a named capture the corpus does not hold", () => {
    const held = "trace-1f2e3d4c-5b6a-4789-8abc-def012345678.pcap.gz"
    const corpus = () => corpusOf([held, "capture_0011.pcap.gz", "capture_0012.pcap.gz"])

    it("passes the names it does hold", async () => {
      expect(await run({ positional: [held], universe: "named" }, undefined, corpus())).toBeNull()
    })

    it("refuses a name it does not, and names the near miss", async () => {
      // One character gone from the middle of a long name.
      const typo = "trace-1f2e3d4c-5b6a-4789-8abc-def01235678.pcap.gz"
      const why = await run({ positional: [typo], universe: "named" }, undefined, corpus())
      expect(why).toContain("names no capture in")
      expect(why).toContain(`did you mean ${held}?`)
    })

    it("refuses a bare stem, a pasted path and an absolute path", async () => {
      const paths = corpus()
      for (
        const name of [
          "trace-1f2e3d4c-5b6a-4789-8abc-def012345678",
          `../../../work/bulk/${held}`,
          `/somewhere/checkout/work/bulk/${held}`
        ]
      ) {
        expect(await run({ positional: [name], universe: "named" }, undefined, paths))
          .toContain("names no capture in")
      }
    })

    it("says nothing about a derived universe, whose names came from the corpus itself", async () => {
      const paths = corpusOf([])
      for (const universe of ["all", "standing", "known"] as const) {
        expect(await run({ positional: ["nonsense.pcap.gz"], universe }, undefined, paths)).toBeNull()
      }
    })
  })
})

describe("the population a sweep states", () => {
  const selection = (over: Partial<Selection>): Selection => ({
    universe: "named",
    positional: [],
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases: [],
    ...over
  })

  it("names the universe and every predicate, in the order they apply", () => {
    expect(stated(selection({ universe: "all", maxSpanSeconds: 120, sampleSize: 1000 })))
      .toBe("--all --max-span 120 --sample 1000")
    expect(stated(selection({ positional: ["a.pcap.gz"], cases: ["a.pcap.gz-case1"] })))
      .toBe("a.pcap.gz --case a.pcap.gz-case1")
  })

  it("names a long capture list by its count and content, so two lists of one size read apart", () => {
    const many = Array.from({ length: 6 }, (_, at) => `c${at}.pcap.gz`)
    const other = [...many.slice(0, 5), "z.pcap.gz"]
    expect(stated(selection({ positional: many }))).toMatch(/^6 named captures #[0-9a-f]{12}$/)
    expect(stated(selection({ positional: many }))).toBe(stated(selection({ positional: [...many].reverse() })))
    expect(stated(selection({ positional: other }))).not.toBe(stated(selection({ positional: many })))
  })
})
