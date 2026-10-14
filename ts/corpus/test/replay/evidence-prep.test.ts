/**
 * Which prep the evidence command replays AS OF, against a real store. The full set
 * and a case set alike reuse the standing cut while it is current under the
 * campaign's staleness rule, applied to one capture; a stale or never-cut
 * capture is cut, and `--force-recut` cuts a current one again. The work
 * list of a derived universe is read against every run the store holds.
 *
 * The cut itself is a fake that opens a prep row and records who asked: the
 * claim is about WHEN the cut runs and which prep comes back, not about
 * cutting. The suite builds and drops its OWN database
 * (`@sip/store/test-server`).
 */
import { Client, Cuts, Model, Population, Runs, Sessions } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest"
import { type Cutter, prepOf, workAsOf } from "../../src/replay/evidence-prep.js"
import * as Fingerprint from "../../src/fingerprint.js"
import type { Options } from "../../src/options.js"
import { workList } from "../../src/universe.js"
import { host, pathsOf } from "../cut/host.js"

const SCRATCH = await scratch("corpus_evidence_prep_test")

const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "evidence-prep-test-"))

const withStore = <A, E, R>(effect: Effect.Effect<A, E, R>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Cuts.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Population.layer),
      Effect.provide(Sessions.layer(ROOT)),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH))),
      Effect.provide(host(ROOT)),
      Effect.orDie
    ) as Effect.Effect<A>
  )

const LANE = "paused"
const CURRENT = "prep_current.pcap.gz"
const STALE = "prep_stale.pcap.gz"
const PROBED = "prep_probed_only.pcap.gz"
/** Cut nowhere, run nowhere: the store has never heard of it. */
const NEVER = "prep_never_cut.pcap.gz"
/** Cut on this code, and the cut BROKE: a reading with no cases to replay. */
const BROKEN = "prep_broken.pcap.gz"
/** Cut on this code, and the rules refused every call: a `no-cases` reading, not a broken one. */
const REFUSED = "prep_refused.pcap.gz"
/** Cut on old code with one call over a 120 s bound; the re-cut brings that call under it. */
const SPANNED = "prep_spanned.pcap.gz"

const caseOf = (id: string, spanMs?: number): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@key`,
  document: { case: { id }, actors: [], ...(spanMs === undefined ? {} : { timing: { capture_span_ms: spanMs } }) },
  assets: [],
  ledger: [{ number: "1009", document: {} }],
  egressEndpoint: null
})

/** `SPANNED` cut under `fingerprint` with its two calls at these spans. */
const cutSpanned = (fingerprint: string, shortMs: number, longMs: number) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const prep = yield* cuts.open(undefined, fingerprint)
    yield* cuts.record({
      prepId: prep,
      capture: SPANNED,
      flows: Model.wholeFlows(new TextEncoder().encode(`{"capture":"${SPANNED}"}`)),
      cases: [caseOf(`${SPANNED}-short`, shortMs), caseOf(`${SPANNED}-long`, longMs)],
      cutFingerprint: fingerprint,
      broke: null,
      refusals: []
    })
    yield* cuts.finish(prep)
    return prep
  })

/**
 * One capture cut under a prep carrying `fingerprint`; the prep's number. The
 * reading holds one case, or none where the cut broke or refused every call.
 */
const cutAs = (
  capture: string,
  fingerprint: string,
  reading: { readonly broke: string | null; readonly cases: number } = { broke: null, cases: 1 }
) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const prep = yield* cuts.open(undefined, fingerprint)
    yield* cuts.record({
      prepId: prep,
      capture,
      flows: Model.wholeFlows(new TextEncoder().encode(`{"capture":"${capture}"}`)),
      cases: reading.cases === 0 ? [] : [caseOf(`${capture}-auto`)],
      cutFingerprint: fingerprint,
      broke: reading.broke,
      refusals: []
    })
    yield* cuts.finish(prep)
    return prep
  })

/** One finished run of `capture` on the lane, of the named set kind. */
const ran = (capture: string, setKind: "full" | "cases") =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({ capture, lane: LANE, mode: "record", sessionDir: path.join(ROOT, capture), setKind })
    yield* runs.finish({
      id: opened.id,
      status: "done",
      cases: [{
        caseId: `${capture}-auto`,
        passed: true,
        verdict: "ok",
        skipped: null,
        failedStep: null,
        rfcGating: 0,
        mainError: null,
        classification: null,
        document: null
      }]
    })
  })

const count = (table: "preps" | "runs") =>
  withStore(
    Effect.gen(function*() {
      const sql = yield* SqlClient
      const rows = yield* sql.unsafe<{ n: string }>(`SELECT count(*) AS n FROM ${table}`)
      return Number(rows[0]!.n)
    })
  )

let currentPrep = 0
let refusedPrep = 0
/** A corpus with no ingest record: nothing there is newer than any cut. */
const corpus = fs.mkdtempSync(path.join(ROOT, "corpus-"))

const options = (capture: string, cases: ReadonlyArray<string>, over: Partial<Options> = {}): Options => ({
  paths: { ...pathsOf(ROOT), corpus },
  lane: LANE,
  selection: {
    universe: "named",
    positional: [capture],
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases
  },
  batchSize: 1000,
  concurrency: 4,
  startsPerSecond: 0,
  memoryMax: "1G",
  patience: { waitMs: 0 },
  dryRun: false,
  prep: undefined,
  cleanup: false,
  forceRecut: false,
  ...over
})

/** Every capture list the fake cut was asked for. */
let cutAsked: Array<ReadonlyArray<string>> = []

const fakeCut: Cutter = (captures) =>
  Effect.gen(function*() {
    cutAsked.push(captures)
    const cuts = yield* Cuts.Service
    const prep = yield* cuts.open(undefined, yield* Fingerprint.cut)
    yield* cuts.finish(prep)
    return { prep }
  })

const ask = (capture: string, cases: ReadonlyArray<string>, over: Partial<Options> = {}) =>
  withStore(prepOf([capture], options(capture, cases, over), fakeCut))

/** Several captures named whole on one line. */
const askSet = (captures: ReadonlyArray<string>) =>
  withStore(
    prepOf(captures, { ...options(captures[0]!, []), selection: { ...options(captures[0]!, []).selection, positional: captures } }, fakeCut)
  )

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
  currentPrep = await withStore(
    Effect.gen(function*() {
      yield* cutAs(STALE, "a-cut-code-that-is-no-more")
      yield* cutSpanned("a-cut-code-that-is-no-more", 5_000, 200_000)
      yield* cutAs(BROKEN, yield* Fingerprint.cut, { broke: "the extractor died", cases: 0 })
      refusedPrep = yield* cutAs(REFUSED, yield* Fingerprint.cut, { broke: null, cases: 0 })
      yield* cutAs(PROBED, yield* Fingerprint.cut)
      const prep = yield* cutAs(CURRENT, yield* Fingerprint.cut)
      yield* ran(CURRENT, "full")
      yield* ran(STALE, "full")
      yield* ran(REFUSED, "full")
      yield* ran(PROBED, "cases")
      return prep
    })
  )
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 60_000)

beforeEach(() => {
  cutAsked = []
})

describe.skipIf(!served)("the prep evidence replays as of", () => {
  it("is the standing prep for a current full set: no cut, no new prep", async () => {
    const before = await count("preps")
    const answer = await ask(CURRENT, [])
    expect(answer).toEqual({ _tag: "prep", prep: currentPrep })
    expect(cutAsked).toEqual([])
    expect(await count("preps")).toBe(before)
  })

  it("is a new prep for a stale full set, and the cut is asked for that capture", async () => {
    const before = await count("preps")
    const answer = await ask(STALE, [])
    expect(answer._tag).toBe("prep")
    expect(answer._tag === "prep" && answer.prep > currentPrep).toBe(true)
    expect(cutAsked).toEqual([[STALE]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("is a cut for a full set whose standing cut broke, current code or not", async () => {
    const before = await count("preps")
    const answer = await ask(BROKEN, [])
    expect(answer._tag).toBe("prep")
    expect(cutAsked).toEqual([[BROKEN]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("is the standing prep for a refused full set: no cases is not broke, and is not recut", async () => {
    const before = await count("preps")
    const answer = await ask(REFUSED, [])
    expect(answer).toEqual({ _tag: "prep", prep: refusedPrep })
    expect(cutAsked).toEqual([])
    expect(await count("preps")).toBe(before)
  })

  it("is a cut for a full set never cut", async () => {
    const before = await count("preps")
    const answer = await ask(NEVER, [])
    expect(answer._tag).toBe("prep")
    expect(cutAsked).toEqual([[NEVER]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("cuts a whole named set, current captures included, when any of it is stale", async () => {
    const before = await count("preps")
    const answer = await askSet([CURRENT, STALE])
    expect(answer._tag).toBe("prep")
    expect(answer._tag === "prep" && answer.prep > currentPrep).toBe(true)
    expect(cutAsked).toEqual([[CURRENT, STALE]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("replays a named set of current captures off the highest of their standing preps, with no cut", async () => {
    const before = await count("preps")
    const answer = await askSet([PROBED, CURRENT])
    expect(answer).toEqual({ _tag: "prep", prep: currentPrep })
    expect(cutAsked).toEqual([])
    expect(await count("preps")).toBe(before)
  })

  it("cuts a current full set again under --force-recut", async () => {
    const before = await count("preps")
    const answer = await ask(CURRENT, [], { forceRecut: true })
    expect(answer._tag).toBe("prep")
    expect(answer._tag === "prep" && answer.prep > currentPrep).toBe(true)
    expect(cutAsked).toEqual([[CURRENT]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("is the standing prep for a case set whose cut is current: no new prep, no cut", async () => {
    const before = await count("preps")
    const answer = await ask(CURRENT, [`${CURRENT}-auto`])
    expect(answer).toEqual({ _tag: "prep", prep: currentPrep })
    expect(cutAsked).toEqual([])
    expect(await count("preps")).toBe(before)
  })

  it("is a new prep for a case set whose cut is stale, and the cut is asked for that capture", async () => {
    const before = await count("preps")
    const answer = await ask(STALE, [`${STALE}-auto`])
    expect(answer._tag).toBe("prep")
    expect(answer._tag === "prep" && answer.prep > currentPrep).toBe(true)
    expect(cutAsked).toEqual([[STALE]])
    expect(await count("preps")).toBe(before + 1)
  })

  it("answers a case set of a capture no full run ran with its prep, a cases run being a run", async () => {
    const answer = await ask(PROBED, [`${PROBED}-auto`])
    expect(answer._tag).toBe("prep")
  })
})

describe.skipIf(!served)("the work list evidence replays, as of that prep", () => {
  const bounded = options(SPANNED, [], { selection: { ...options(SPANNED, []).selection, maxSpanSeconds: 120 } })
  /** A cut that brings the long call under the bound: what a re-cut may do to a span-derived set. */
  const recut: Cutter = () =>
    Effect.map(Effect.orDie(Effect.flatMap(Fingerprint.cut, (code) => cutSpanned(code, 5_000, 50_000))), (prep) => ({ prep }))

  it("is asked again under a span bound, off the prep the cut opened", async () => {
    const before = await withStore(workList(bounded.paths, LANE, bounded.selection, undefined))
    expect(before).toEqual([{ capture: SPANNED, set: { kind: "cases", cases: [`${SPANNED}-short`] } }])
    const prep = await withStore(prepOf([SPANNED], bounded, recut))
    expect(prep._tag).toBe("prep")
    const after = await withStore(workAsOf(before, bounded, prep._tag === "prep" ? prep.prep : 0))
    // The stale ids and spans are not what the new prep cut: both calls are under the bound now.
    expect(after).toEqual([{ capture: SPANNED, set: { kind: "full" } }])
  })

  it("reads `--all --only-new` as the captures the lane never ran, whoever ran the others", async () => {
    // The runs of the suite carry no recorded env: none is @develop, and each still ran on the lane.
    const fresh = "prep_never_run.pcap.gz"
    await withStore(Effect.flatMap(Population.Service, (population) =>
      population.add([{ capture: fresh, sourcePath: null }, { capture: CURRENT, sourcePath: null }])))
    const all = options(CURRENT, [], {
      selection: { ...options(CURRENT, []).selection, universe: "all", positional: [] }
    })
    const everything = await withStore(workList(all.paths, LANE, all.selection, undefined))
    expect(everything.map((work) => work.capture)).toEqual(expect.arrayContaining([fresh, CURRENT, STALE, PROBED]))
    const onlyNew = { ...all.selection, onlyNew: true }
    expect((await withStore(workList(all.paths, LANE, onlyNew, undefined))).map((work) => work.capture))
      .toEqual([fresh])
  })

  it("is kept as read where no bound was stated", async () => {
    const plain = options(CURRENT, [])
    const before = await withStore(workList(plain.paths, LANE, plain.selection, undefined))
    expect(await withStore(workAsOf(before, plain, currentPrep + 1))).toBe(before)
  })
})
