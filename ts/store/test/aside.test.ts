/**
 * A run recorded ASIDE (env `aside`) is reached by naming it and never by a
 * reading that chooses a run: it is no capture a lane already ran, no lane's
 * count, no call's latest or recent run, and in no run set but one naming its
 * run or its environment. Against a real postgres.
 *
 * The suite builds and drops its OWN database (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as CallHistory from "../src/call-history.js"
import * as Client from "../src/client.js"
import type { CaseResult } from "../src/model.js"
import type { RunIdentity } from "../src/run-identity.js"
import * as RunSets from "../src/run-sets.js"
import * as Runs from "../src/runs.js"
import * as Selector from "../src/selector.js"
import * as Sessions from "../src/sessions.js"
import * as Server from "./server.js"
import { CURRENT, TASK_ENV } from "./identity.js"

const SCRATCH = await Server.scratch("store_aside_test")

type Services = Runs.Service | RunSets.Service | CallHistory.Service

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Runs.layer, RunSets.layer, CallHistory.layer]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

/** The identity an aside run is recorded under, spelled as the store reads it. */
const ASIDE: RunIdentity = { env: "aside", branch: null, laneHash: null, load: null }

const LANE = "fake"

const result = (caseId: string, passed: boolean): CaseResult => ({
  caseId,
  passed,
  verdict: passed ? "ok" : "failed",
  skipped: null,
  failedStep: passed ? null : "s3",
  rfcGating: 0,
  mainError: null,
  classification: null,
  document: null
})

const ran = (capture: string, cases: ReadonlyArray<CaseResult>, identity: RunIdentity, setKind?: "cases") =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({
      capture,
      lane: LANE,
      mode: "record",
      sessionDir: `/w/${capture}`,
      identity,
      ...(setKind === undefined ? {} : { setKind })
    })
    yield* runs.finish({ id: opened.id, status: "done", cases })
    return opened.id
  })

const selector = (text: string): Selector.Selector => {
  const parsed = Selector.parse(text)
  if (Selector.isRefused(parsed)) throw new Error(parsed.refused)
  return parsed
}

beforeAll(async () => {
  if (!Server.served) return
  await Server.fresh(SCRATCH)
  await Server.on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!Server.served) return
  await Server.drop(SCRATCH)
}, 60_000)

describe.skipIf(!Server.served)("a run recorded aside", () => {
  const ONLY = "aside_only.pcap.gz"
  const BOTH = "aside_both.pcap.gz"
  let ids: { readonly current: number; readonly task: number; readonly aside: number; readonly only: number }

  beforeAll(async () => {
    if (!Server.served) return
    ids = await withStore(Effect.gen(function*() {
      const current = yield* ran(BOTH, [result(`${BOTH}-k1`, false)], CURRENT)
      const task = yield* ran(BOTH, [result(`${BOTH}-k1`, false)], TASK_ENV)
      const aside = yield* ran(BOTH, [result(`${BOTH}-k1`, true)], ASIDE, "cases")
      const only = yield* ran(ONLY, [result(`${ONLY}-k1`, true)], ASIDE, "cases")
      return { current, task, aside, only }
    }))
  }, 60_000)

  it("is no capture the lane already ran", async () => {
    const ranOn = await withStore(Effect.flatMap(Runs.Service, (runs) => runs.ranOn(LANE)))
    expect(ranOn).toContain(BOTH)
    expect(ranOn).not.toContain(ONLY)
  })

  it("counts in no lane's runs or captures", async () => {
    const lanes = await withStore(Effect.flatMap(Runs.Service, (runs) => runs.lanes))
    expect(lanes.find((lane) => lane.lane === LANE)).toEqual({ lane: LANE, runs: 2, captures: 1 })
  })

  it("is no call's latest or recent run", async () => {
    const history = await withStore(
      Effect.flatMap(CallHistory.Service, (calls) => calls.ofCall(BOTH, `${BOTH}-k1`, [LANE], 10))
    )
    expect(history.latest.map((entry) => entry.runId)).toEqual([ids.task])
    expect(history.recent.map((entry) => entry.runId)).toEqual([ids.task, ids.current])
  })

  it("is in no run set that does not name its run or its environment", async () => {
    const runsOf = (text: string) =>
      withStore(Effect.flatMap(RunSets.Service, (sets) => sets.runs(selector(text), 10))).then((rows) =>
        rows.map((row) => row.id)
      )
    expect(await runsOf(`@lane:${LANE}`)).toEqual([ids.task, ids.current])
    expect(await runsOf(`@run:${ids.aside}`)).toEqual([ids.aside])
    expect(await runsOf("@env:aside")).toEqual([ids.only, ids.aside])
  })
})
