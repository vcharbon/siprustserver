/**
 * A job's collectors around its work, real executables against a real
 * postgres: what they answer of a landed cell is merged into it, of the job
 * are its lines; a job interrupted after they opened still closes them; a
 * close that fails twice marks the job lab-left-changed.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { type Lanes, RigPath } from "@sip/rig"
import { Attachments, CellAnswers, Client, JobEvents, Jobs, Runs, Sessions, Verifications } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { Collectors, Roots } from "../../src/deployment/index.js"
import * as JobCollectors from "../../src/replay/job-collectors.js"

const SCRATCH = await scratch("corpus_job_collectors_test")
const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "job-collectors-"))
const LANE = "lane-a"

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(TREE, { recursive: true, force: true })
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

/** The deployment's collectors: the job collector `lab` on {@link LANE}, its steps the scripts given, its trace file. */
const labOf = async (steps: { readonly collect: string; readonly close: string }) => {
  const root = fs.mkdtempSync(path.join(TREE, "rig-"))
  const dir = path.join(root, "collectors", "lab")
  fs.mkdirSync(dir, { recursive: true })
  const trace = path.join(root, "trace")
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "lab"\nscope = "job"\nanswers = ["lab-check"]\n` +
      `[run]\nopen = "./open"\ncollect = "./collect"\nclose = "./close"\n`
  )
  const script = (name: string, body: string) =>
    fs.writeFileSync(path.join(dir, name), `#!/bin/sh\necho ${name} >> "${trace}"\n${body}\n`, { mode: 0o755 })
  script("open", "exit 0")
  script("collect", steps.collect)
  script("close", steps.close)
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: LANE,
    file: `lanes/${LANE}.toml`,
    lane: { schema: "lane.v1", clock: "simulated" },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return { layer: Collectors.layerOf(rig, [lane]), trace: () => fs.readFileSync(trace, "utf8").split("\n").filter((line) => line !== "") }
}

type Services =
  | Attachments.Service
  | CellAnswers.Service
  | Collectors.Service
  | JobEvents.Service
  | Jobs.Service
  | Roots.Service
  | Runs.Service
  | Verifications.Service
  | FileSystem.FileSystem
  | SqlClient

const withStore = <A, E>(collectors: Layer.Layer<Collectors.Service>, effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Layer.mergeAll(Attachments.layer, CellAnswers.layer, JobEvents.layer, Jobs.layer, Runs.layer, Verifications.layer)),
      Effect.provide(Layer.mergeAll(collectors, Roots.layer, Sessions.layer(TREE), NodeServices.layer)),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    ) as Effect.Effect<A, E>
  )

const jobOf = Effect.flatMap(Jobs.Service, (jobs) => jobs.create({ kind: "campaign", args: {}, createdBy: "test" }))

/** A run whose one cell `case-1` landed passing, its bundle stating its clock and window; the cell as the job ran it. */
const landedCell = Effect.gen(function*() {
  const runs = yield* Runs.Service
  const bundle = fs.mkdtempSync(path.join(TREE, "run-"))
  const dir = path.join(bundle, "c1")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, "run-config.json"), `{"claim_numbers":{},"clock":"virtual","identities":{},"lane":"${LANE}","route_target":"127.0.0.1:5080"}\n`)
  fs.writeFileSync(path.join(dir, "timing.json"), `{"settle_budget_ms":100,"settled_at_ms":10,"started_at_ms":0,"wall_started_at_ms":1,"wall_ended_at_ms":2}\n`)
  const opened = yield* runs.open({ capture: "jobs.pcap", lane: LANE, mode: "record", sessionDir: bundle, identity: CURRENT })
  yield* runs.finish({
    id: opened.id,
    status: "done",
    cases: [{
      caseId: "case-1",
      passed: true,
      verdict: "ok",
      skipped: null,
      failedStep: null,
      rfcGating: null,
      mainError: null,
      classification: null,
      document: null,
      verifications: [{ name: "call", source: "interpreter", answer: "pass" }]
    }]
  })
  return { id: "c1", caseId: "case-1", lane: LANE, dir, runId: opened.id }
})

describe.skipIf(!served)("a job's collectors", () => {
  it("answer the landed cells and the job once its work ended, then close", async () => {
    const lab = await labOf({
      collect: [
        `echo '{"verification":"lab-check","cell":"c1","answer":"fail","message":"the lab saw it"}'`,
        `echo '{"verification":"lab-check","answer":"pass"}'`
      ].join("\n"),
      close: "exit 0"
    })
    const { cellLines, jobLines } = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const cell = yield* landedCell
      yield* JobCollectors.around({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [cell] }, Effect.void)
      const lines = yield* Verifications.Service
      return {
        cellLines: yield* lines.of({ scope: "cell", runId: cell.runId, caseId: cell.caseId }),
        jobLines: yield* lines.of({ scope: "job", jobId: job })
      }
    }))
    expect(cellLines).toEqual([
      { name: "call", source: "interpreter", answer: "pass" },
      { name: "lab-check", source: "lab", answer: "fail", message: "the lab saw it" }
    ])
    expect(jobLines).toEqual([{ name: "lab-check", source: "lab", answer: "pass" }])
    expect(lab.trace()).toEqual(["open", "collect", "close"])
  })

  it("close where the job is interrupted after they opened", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0" })
    await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const work = Effect.andThen(Effect.sync(() => fs.writeFileSync(path.join(TREE, `working${job}`), "")), Effect.never)
      const fiber = yield* Effect.forkChild(
        JobCollectors.around({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [] }, work)
      )
      yield* Effect.promise(() =>
        new Promise<void>((resolve) => {
          const poll = setInterval(() => {
            if (fs.existsSync(path.join(TREE, `working${job}`))) {
              clearInterval(poll)
              resolve()
            }
          }, 10)
        })
      )
      yield* Fiber.interrupt(fiber)
    }))
    expect(lab.trace()).toEqual(["open", "close"])
  })

  it("mark the job lab-left-changed where a close fails twice", async () => {
    const lab = await labOf({ collect: "exit 0", close: "echo 'the lab would not reset' >&2\nexit 1" })
    const marked = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      yield* JobCollectors.around({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [] }, Effect.void)
      return yield* Effect.flatMap(JobEvents.Service, (events) => events.labLeftChanged(job))
    }))
    expect(marked).toBe(true)
    expect(lab.trace()).toEqual(["open", "collect", "close", "close"])
  })
})
