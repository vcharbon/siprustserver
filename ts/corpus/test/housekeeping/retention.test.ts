/**
 * The retention ruling against a real postgres: a real-clock
 * lane's bundle is kept, a paused-clock bundle is kept while it is a call's
 * current run's, while it is pinned or cited, while its run is open, or while
 * it is younger than the grace — and every other one goes. A bundle no row
 * names is named and kept, or dropped where the pass is told to forget them.
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { Client, type Model, Pins, type RunIdentity, Runs, Sessions } from "@sip/store"
import { CURRENT_POPULATION } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest"
import * as Retention from "../../src/housekeeping/retention.js"

const SCRATCH = await scratch("corpus_retention_test")

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

let sessions: string

beforeEach(() => {
  sessions = fs.mkdtempSync(path.join(os.tmpdir(), "retention-test-"))
})
afterEach(() => {
  fs.rmSync(sessions, { recursive: true, force: true })
})

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Runs.Service | Pins.Service | SqlClient | FileSystem.FileSystem>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Layer.mergeAll(Runs.layer, Pins.layer).pipe(
        Layer.provideMerge(
          Layer.mergeAll(Client.layerFromUrl(urlFor(SCRATCH)), Sessions.layer(sessions), NodeServices.layer)
        )
      ))
    )
  )

const passed = (caseId: string): Model.CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null
})

const CURRENT: RunIdentity.RunIdentity = { ...CURRENT_POPULATION, laneHash: null, load: null }
const ENV: RunIdentity.RunIdentity = { env: "task-9", branch: "task/9", laneHash: null, load: null }

/** One run with its bundle on disk, finished `hoursAgo` hours ago, or left open. */
const ran = (
  capture: string,
  lane: string,
  identity: RunIdentity.RunIdentity,
  hoursAgo: number | "open"
) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const sql = yield* SqlClient
    const opened = yield* runs.open({
      capture,
      lane,
      mode: "record",
      sessionDir: path.join(sessions, capture.replace(".pcap.gz", "")),
      identity
    })
    if (hoursAgo !== "open") {
      yield* runs.finish({ id: opened.id, status: "done", cases: [passed(`${capture}-auto`)] })
      yield* sql`UPDATE runs SET finished_at = now() - make_interval(hours => ${hoursAgo}) WHERE id = ${opened.id}`
    }
    fs.mkdirSync(opened.bundlePath, { recursive: true })
    return opened
  })

const GRACE = 24

describe.skipIf(!served)("the retention ruling", () => {
  it("keeps a real-clock lane's bundle, the current run's, a pinned one, a cited one, an open one and a young one; drops the rest", async () => {
    const at = await withStore(Effect.gen(function*() {
      const real = yield* ran("ret_real.pcap.gz", "real", ENV, 500)
      const superseded = yield* ran("ret_cur.pcap.gz", "paused", CURRENT, 300)
      const current = yield* ran("ret_cur.pcap.gz", "paused", CURRENT, 200)
      const envOld = yield* ran("ret_env.pcap.gz", "paused", ENV, 100)
      const envYoung = yield* ran("ret_env.pcap.gz", "paused", ENV, 2)
      const pinned = yield* ran("ret_pin.pcap.gz", "paused", ENV, 100)
      const open = yield* ran("ret_open.pcap.gz", "paused", ENV, "open")
      const cited = yield* ran("ret_cite.pcap.gz", "paused", ENV, 100)
      yield* Effect.flatMap(Pins.Service, (pins) => pins.pin([pinned.id], "proj/1"))
      const sql = yield* SqlClient
      yield* sql`INSERT INTO citations (kind, ref, cited_by) VALUES ('run', ${String(cited.id)}, 'proj/2')`
      return { real, superseded, current, envOld, envYoung, pinned, open, cited }
    }))
    const unrecorded = path.join(sessions, "ret_gone", "run999999")
    fs.mkdirSync(unrecorded, { recursive: true })
    const plan = await withStore(Retention.plan({
      sessions,
      livePreps: null,
      graceHours: GRACE,
      realLanes: ["real", "managed"],
      forgetUnrecorded: false
    }))
    expect([...plan.removable].sort()).toEqual([at.superseded.bundlePath, at.envOld.bundlePath].sort())
    expect(plan.unrecorded).toEqual([unrecorded])
    const forgetting = await withStore(Retention.plan({
      sessions,
      livePreps: null,
      graceHours: GRACE,
      realLanes: ["real", "managed"],
      forgetUnrecorded: true
    }))
    expect([...forgetting.removable].sort()).toEqual([at.superseded.bundlePath, at.envOld.bundlePath, unrecorded].sort())
  })
})
