/**
 * A cut run in the foreground as a job: the row a campaign's cut leaves behind
 * — kind `cut`, running under this pid, its captures planned, the prep opened
 * under it, closed with the cut's verdict — so the jobs list shows it as it shows a
 * launched one.
 */
import { CaseIds, Cuts, Jobs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import type { CutReading } from "../../src/cut/cut-contract.js"
import * as CutJob from "../../src/cut/cut-job.js"
import { NOTHING_MIRRORED } from "../../src/cut/cut-rows.js"
import type { Options } from "../../src/options.js"
import { host as hostOf, pathsOf } from "./host.js"

const host = hostOf("/w/tree")

const options: Options = {
  paths: { ...pathsOf("/w/tree"), sessions: "/w/sessions" },
  lane: "paused",
  selection: {
    universe: "named",
    positional: [],
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases: []
  },
  batchSize: 1000,
  concurrency: 2,
  startsPerSecond: 0,
  memoryMax: "3G",
  patience: { waitMs: 0 },
  dryRun: false,
  prep: undefined,
  cleanup: false,
  forceRecut: false
}

const JOB = 42
const PREP = 7

interface Store {
  readonly layer: Layer.Layer<Cuts.Service | Jobs.Service | CaseIds.Service>
  readonly created: Array<Jobs.CreateRequest>
  readonly started: Array<[number, Jobs.StartedRequest]>
  readonly planned: Array<[number, number]>
  readonly closed: Array<[number, Jobs.CloseRequest]>
  readonly openedUnder: Array<number | undefined>
  readonly finished: Array<number>
}

const store = (): Store => {
  const created: Store["created"] = []
  const started: Store["started"] = []
  const planned: Store["planned"] = []
  const closed: Store["closed"] = []
  const openedUnder: Store["openedUnder"] = []
  const finished: Store["finished"] = []
  const refuse = Effect.die(new Error("the cut job asked the store the wrong question"))
  const jobs = Layer.succeed(
    Jobs.Service,
    Jobs.Service.of({
      create: (request) =>
        Effect.sync(() => {
          created.push(request)
          return JOB
        }),
      started: (id, request) => Effect.sync(() => void started.push([id, request])),
      close: (id, request) => Effect.sync(() => void closed.push([id, request])),
      waiting: () => refuse,
      batch: () => refuse,
      sweep: () => refuse,
      stop: () => refuse,
      planned: (id, units) => Effect.sync(() => void planned.push([id, units])),
      settle: () => refuse,
      show: () => refuse,
      explain: () => refuse,
      list: () => refuse,
      page: () => refuse,
      lineage: () => refuse,
      gone: () => refuse,
      red: () => refuse,
      running: refuse
    })
  )
  const cuts = Layer.succeed(
    Cuts.Service,
    Cuts.Service.of({
      open: (_provenance: unknown, _fingerprint: string, jobId?: number) =>
        Effect.sync(() => {
          openedUnder.push(jobId)
          return PREP
        }),
      finish: (prep: number) => Effect.sync(() => void finished.push(prep))
    } as unknown as Cuts.Interface)
  )
  const ids = Layer.succeed(
    CaseIds.Service,
    CaseIds.Service.of({
      of: () => refuse,
      freeze: () => refuse,
      markSeeded: () => refuse,
      unseeded: () => refuse,
      count: refuse
    })
  )
  return { layer: Layer.mergeAll(jobs, cuts, ids), created, started, planned, closed, openedUnder, finished }
}

const reading = (capture: string, broke?: string): CutReading => ({
  capture,
  prep: PREP,
  cutAt: "2026-09-20T00:00:00.000Z",
  flows: "flows.json",
  excluded: "excluded.json",
  cutFingerprint: "cut-code",
  provenance: [],
  cases: [{ id: "c1", dir: "/w/c1" }],
  ...(broke === undefined ? {} : { broke })
})

/** A census gate that lets every cut through. */
const passing = () => Effect.succeed("census.json")

/** A phase that records what it was asked and answers these readings. */
const phaseOf = (readings: ReadonlyArray<CutReading>, asked: Array<[ReadonlyArray<string>, number | undefined, number | undefined]>): CutJob.Phase =>
  (captures, _options, standing, jobId) =>
    Effect.sync(() => {
      asked.push([captures, standing, jobId])
      return { prep: standing ?? PREP, readings: new Map(readings.map((one) => [one.capture, one])), mirrored: NOTHING_MIRRORED }
    })

describe("CutJob.run", () => {
  it("runs as a cut job of this process: row created and started under the pid, captures planned, prep under the job, closed done", async () => {
    const on = store()
    const asked: Array<[ReadonlyArray<string>, number | undefined, number | undefined]> = []
    const captures = ["a.pcap.gz", "b.pcap.gz"]
    const done = await Effect.runPromise(
      CutJob.run(captures, options, "campaign", phaseOf(captures.map((one) => reading(one)), asked), passing).pipe(
        Effect.provide(Layer.merge(on.layer, host))
      )
    )
    expect(done).toMatchObject({ prep: PREP, job: JOB })
    expect(on.created).toHaveLength(1)
    expect(on.created[0]).toMatchObject({
      kind: "cut",
      args: { captures, mode: "stale-only", concurrency: 2, memoryMax: "3G" }
    })
    expect(on.started).toEqual([[JOB, { pid: process.pid, hooksPath: "/w/sessions/_jobs/42/hooks" }]])
    expect(on.planned).toEqual([[JOB, 2]])
    expect(on.openedUnder).toEqual([JOB])
    expect(asked).toEqual([[captures, PREP, JOB]])
    expect(on.finished).toEqual([PREP])
    expect(on.closed).toEqual([[JOB, { status: "done", exit: 0 }]])
  })

  it("closes the row failed where a capture broke, the prep finished all the same", async () => {
    const on = store()
    const done = await Effect.runPromise(
      CutJob.run(["a.pcap.gz"], options, "campaign", phaseOf([reading("a.pcap.gz", "[prep7] the cut failed: boom")], []), passing).pipe(
        Effect.provide(Layer.merge(on.layer, host))
      )
    )
    expect(done.job).toBe(JOB)
    expect(on.finished).toEqual([PREP])
    expect(on.closed).toEqual([[JOB, { status: "failed", exit: 1 }]])
  })

  it("closes the row failed before a defect in the phase propagates", async () => {
    const on = store()
    const broken: CutJob.Phase = () => Effect.die(new Error("the cut fell over"))
    const exit = await Effect.runPromiseExit(
      CutJob.run(["a.pcap.gz"], options, "campaign", broken, passing).pipe(Effect.provide(Layer.merge(on.layer, host)))
    )
    expect(exit._tag).toBe("Failure")
    expect(on.closed).toEqual([[JOB, { status: "failed", exit: 1 }]])
    expect(on.finished).toEqual([])
  })
})
