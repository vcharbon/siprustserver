/**
 * A run's setup operations on its campaign job: each phase an event of the job
 * in the store, the job marked when a heal fails twice and only then. The
 * suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { JobEvents, Jobs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest"
import { setupOf, sinkOf } from "../src/setup-phase.js"
import { planned, workspace } from "./setup-operations.js"

const SCRATCH = await scratch("campaign_runner_setup_events_test")

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

const made: Array<string> = []
afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const withStore = <A, E>(effect: Effect.Effect<A, E, Jobs.Service | JobEvents.Service>) =>
  Effect.runPromise(
    effect.pipe(Effect.provide(Jobs.layer), Effect.provide(JobEvents.layer), Effect.provide(layerFromUrl(urlFor(SCRATCH))))
  )

/** A campaign job whose setup `names` ran around a job that did nothing: whether it is marked, and its events as `<phase> <subject>`. */
const ranOnJob = (names: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const root = workspace()
    made.push(root)
    const instances = yield* Effect.promise(() => planned(root, names))
    const jobs = yield* Jobs.Service
    const events = yield* JobEvents.Service
    const id = yield* jobs.create({ kind: "campaign", args: {}, createdBy: "setup-events" })
    const healed = yield* setupOf(instances, sinkOf({ id, events }), () => Effect.void)(Effect.void)
    const rows = yield* events.ofJob(id)
    return {
      healed,
      marked: yield* events.labLeftChanged(id),
      phases: rows.map((row) => `${row.phase} ${row.subject}`),
      messages: rows.flatMap((row) => (row.message === null ? [] : [row.message]))
    }
  })

describe.skipIf(!served)("a run's setup on its campaign job", () => {
  it("writes each phase as an event of the job, and leaves a job whose heals passed unmarked", async () => {
    const { healed, marked, phases } = await withStore(ranOnJob(["restart-worker"]))
    expect(healed.labLeftChanged).toBe(false)
    expect(marked).toBe(false)
    expect(phases).toEqual([
      "inject restart-worker",
      "injected restart-worker",
      "heal restart-worker",
      "healed restart-worker",
      "recovered restart-worker"
    ])
  })

  it("marks the job whose operation's heal failed twice", async () => {
    const { healed, marked, phases, messages } = await withStore(ranOnJob(["stuck"]))
    expect(healed.labLeftChanged).toBe(true)
    expect(marked).toBe(true)
    expect(phases.slice(-1)).toEqual(["lab-left-changed stuck"])
    expect(messages.at(-1)).toMatch(/^2 heal attempts failed, the last: heal exited 1, its log /)
  })
})
