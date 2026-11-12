/**
 * A job's collectors around its work, real executables against a real
 * postgres: what they answer of a landed cell is merged into it, of the job
 * are its lines; a job interrupted after they opened still closes them; a
 * close that fails twice marks the job lab-left-changed. A job's pre-checks
 * refuse it with a fail's message, their lines kept before its collectors';
 * a real-clock lane naming no part a required pre-check needs refuses it.
 * What either attaches to the job is held under its directory in the session
 * tree, whatever directory the caller names.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { type Lanes, RigPath } from "@sip/rig"
import { Attachments, CellAnswers, Client, JobEvents, Jobs, Runs, Sessions, Verifications } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Deferred from "effect/Deferred"
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
import * as Job from "../../src/jobs/job.js"
import * as CollectorIo from "../../src/replay/collector-io.js"
import * as JobCollectors from "../../src/replay/job-collectors.js"
import * as PreChecks from "../../src/replay/pre-checks.js"

const SCRATCH = await scratch("corpus_job_collectors_test")
const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "job-collectors-"))
/** A directory outside the session tree {@link TREE}, as a caller may name the job's. */
const ELSEWHERE = fs.mkdtempSync(path.join(os.tmpdir(), "job-collectors-elsewhere-"))
const LANE = "lane-a"

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(TREE, { recursive: true, force: true })
  fs.rmSync(ELSEWHERE, { recursive: true, force: true })
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

/**
 * The deployment's collectors: the job collector `lab` on {@link LANE}, its
 * steps the scripts given, its default `setting` where one is given, its
 * trace file; and, where `preCheck` is given, the job pre-check `lab-own`
 * whose collect runs it, required and needing the part `needs` where one is
 * named, the lane's clock `clock`, the lane excluding the collectors
 * `exclude`.
 */
const labOf = async (steps: {
  readonly collect: string
  readonly close: string
  readonly locks?: ReadonlyArray<string>
  readonly setting?: string
  readonly preCheck?: string
  readonly needs?: string
  readonly clock?: "real" | "simulated"
  readonly exclude?: ReadonlyArray<string>
}) => {
  const root = fs.mkdtempSync(path.join(TREE, "rig-"))
  const dir = path.join(root, "collectors", "lab")
  fs.mkdirSync(dir, { recursive: true })
  const trace = path.join(root, "trace")
  if (steps.preCheck !== undefined) {
    const own = path.join(root, "collectors", "lab-own")
    fs.mkdirSync(own, { recursive: true })
    fs.writeFileSync(
      path.join(own, "collector.toml"),
      `schema = "collector.v1"\nname = "lab-own"\nscope = "job"\nat = "before"\n` +
        (steps.needs === undefined ? "" : `required = true\nneeds = ["${steps.needs}"]\n`) +
        `[run]\ncollect = "./collect"\n`
    )
    fs.writeFileSync(path.join(own, "collect"), `#!/bin/sh\necho pre-check >> "${trace}"\n${steps.preCheck}\n`, { mode: 0o755 })
  }
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "lab"\nscope = "job"\nanswers = ["lab-check"]\n` +
      (steps.locks === undefined ? "" : `locks = ${JSON.stringify(steps.locks)}\n`) +
      (steps.setting === undefined ? "" : `default = "${steps.setting}"\n`) +
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
    lane: {
      schema: "lane.v1",
      clock: steps.clock ?? "simulated",
      ...(steps.exclude === undefined ? {} : { collectors: { exclude: steps.exclude } as never })
    },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return {
    layer: Collectors.layerOf(rig, [lane]),
    trace: () => (fs.existsSync(trace) ? fs.readFileSync(trace, "utf8").split("\n").filter((line) => line !== "") : [])
  }
}

/** A lock root of its own, as the deployment's roots state it. */
const lockRoot = () => {
  const locks = fs.mkdtempSync(path.join(TREE, "locks-"))
  return {
    locks,
    roots: Roots.layerWith({
      paths: () => Effect.die("unused"),
      pcapRoots: Effect.succeed([]),
      locks: Effect.succeed(locks),
      bench: Effect.succeed(locks)
    })
  }
}

type Services =
  | Attachments.Service
  | CellAnswers.Service
  | Collectors.Service
  | JobEvents.Service
  | Jobs.Service
  | Roots.Service
  | Runs.Service
  | Sessions.Service
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

/** A step writing `job.log` under `SIPRIG_OUT` and attaching it to the job. */
const ATTACHING = `echo job > "$SIPRIG_OUT/job.log"\necho '{"attachment":{"kind":"log","file":"job.log"}}'`

/** What job `job` holds attached: each one's name, source, path and its file's content. */
const jobAttachmentsOf = (job: number) =>
  Effect.map(Effect.flatMap(Attachments.Service, (store) => store.of({ scope: "job", jobId: job })), (stored) =>
    stored.map((one) => ({
      name: one.name,
      source: one.source,
      path: one.path,
      content: one.path === undefined ? undefined : fs.readFileSync(one.path, "utf8")
    })))

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

describe.skipIf(!served)("a job's pre-checks", () => {
  const OTHER_LAB = "the container sut carries the root of another lab"

  it("refuse the job with a fail's message, its line and its refused event the job's", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0", preCheck: `echo '{"verification":"lab-own","answer":"fail","message":"${OTHER_LAB}"}'` })
    const { refused, jobLines, said } = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const refused = yield* PreChecks.job({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE] })
      return {
        refused,
        jobLines: yield* Effect.flatMap(Verifications.Service, (lines) => lines.of({ scope: "job", jobId: job })),
        said: (yield* Effect.flatMap(JobEvents.Service, (events) => events.ofJob(job))).filter((event) => event.phase === "refused")
      }
    }))
    expect(refused).toBe(OTHER_LAB)
    expect(jobLines).toEqual([{ name: "lab-own", source: "lab-own", answer: "fail", message: OTHER_LAB }])
    expect(said.map((event) => [event.subject, event.message])).toEqual([["lab-own", OTHER_LAB]])
    expect(lab.trace()).toEqual(["pre-check"])
  })

  it("keep their lines of the job before those its collectors answer, which never run them again", async () => {
    const lab = await labOf({
      collect: `echo '{"verification":"lab-check","answer":"pass"}'`,
      close: "exit 0",
      preCheck: `echo '{"verification":"lab-own","answer":"pass"}'`
    })
    const { refused, jobLines } = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const dir = path.join(TREE, `job${job}`)
      const refused = yield* PreChecks.job({ job, dir, lanes: [LANE] })
      yield* JobCollectors.around({ job, dir, lanes: [LANE], cells: () => [] }, Effect.void)
      return { refused, jobLines: yield* Effect.flatMap(Verifications.Service, (lines) => lines.of({ scope: "job", jobId: job })) }
    }))
    expect(refused).toBeUndefined()
    expect(jobLines).toEqual([{ name: "lab-own", source: "lab-own", answer: "pass" }, { name: "lab-check", source: "lab", answer: "pass" }])
    expect(lab.trace()).toEqual(["pre-check", "open", "collect", "close"])
  })

  it("attach their files to the job under its directory in the session tree, whatever directory the caller names", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0", preCheck: ATTACHING })
    const { job, attached } = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      yield* PreChecks.job({ job, dir: path.join(ELSEWHERE, `job${job}`), lanes: [LANE] })
      return { job, attached: yield* jobAttachmentsOf(job) }
    }))
    expect(attached).toEqual([
      { name: "lab-own", source: "lab-own", path: path.join(Job.jobDir(TREE, job), "collected", "lab-own", "job.log"), content: "job\n" }
    ])
  })

  it("refuse a real-clock lane that names no part a required pre-check needs, running none", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0", preCheck: "exit 0", needs: "records-store", clock: "real" })
    const refused = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      return yield* PreChecks.job({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE] })
    }))
    expect(refused).toBe(`${LANE} names no records-store part, which the required pre-check lab-own needs (lanes/${LANE}.toml)`)
    expect(lab.trace()).toEqual([])
  })

  it("refuse a job asking for a pre-check its lane excludes, running none", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0", preCheck: "exit 0", exclude: ["lab-own"] })
    const refused = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      return yield* PreChecks.job({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE], asked: ["lab-own"] })
    }))
    expect(refused).toBe(`${LANE} excludes lab-own, a pre-check the job asks for (lanes/${LANE}.toml)`)
    expect(lab.trace()).toEqual([])
  })
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

  it("attach their files to the job under its directory in the session tree, whatever directory the caller names", async () => {
    const lab = await labOf({ collect: ATTACHING, close: "exit 0" })
    const { job, attached } = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      yield* JobCollectors.around({ job, dir: path.join(ELSEWHERE, `job${job}`), lanes: [LANE], cells: () => [] }, Effect.void)
      return { job, attached: yield* jobAttachmentsOf(job) }
    }))
    expect(attached).toEqual([
      { name: "lab", source: "lab", path: path.join(Job.jobDir(TREE, job), "collected", "lab", "job.log"), content: "job\n" }
    ])
  })

  it("state in their contexts the cells their caller plans, and none where it plans none", async () => {
    const seen = path.join(TREE, "planned-context.json")
    const lab = await labOf({ collect: `cp "$SIPRIG_CONTEXT" "${seen}"`, close: "exit 0" })
    const planned = [{ id: "c1", case: "case-1", lane: LANE, claims: { lab: { expects: 1 } } }]
    const contextOf = (stated: typeof planned | undefined) =>
      withStore(lab.layer, Effect.gen(function*() {
        const job = yield* jobOf
        yield* JobCollectors.around(
          { job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [], ...(stated === undefined ? {} : { planned: stated }) },
          Effect.void
        )
        return JSON.parse(fs.readFileSync(seen, "utf8")) as { readonly planned?: unknown }
      }))
    expect((await contextOf(planned)).planned).toEqual(planned)
    expect((await contextOf(undefined)).planned).toBeUndefined()
  })

  it("collect an on-failure collector over a cell its caller states failed, though no line of it failed", async () => {
    const lab = await labOf({
      collect: `echo '{"verification":"lab-check","cell":"c1","answer":"pass"}'`,
      close: "exit 0",
      setting: "on-failure"
    })
    const cellLines = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const cell = yield* landedCell
      yield* JobCollectors.around(
        { job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [{ ...cell, failed: true }] },
        Effect.void
      )
      return yield* Effect.flatMap(Verifications.Service, (store) => store.of({ scope: "cell", runId: cell.runId, caseId: cell.caseId }))
    }))
    expect(lab.trace()).toEqual(["open", "collect", "close"])
    expect(cellLines.map((line) => [line.name, line.answer])).toEqual([["call", "pass"], ["lab-check", "pass"]])
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

  it("wait for a held lock interruptibly where the caller runs them masked, saying whom they wait for", async () => {
    const lab = await labOf({ collect: "exit 0", close: "exit 0", locks: ["lab"] })
    const { locks, roots } = lockRoot()
    const holder = path.join(locks, `${CollectorIo.LOCK_PREFIX}lab.json`)
    fs.writeFileSync(holder, `${JSON.stringify({ pid: process.pid, since: new Date().toISOString(), what: "another job's collector" })}\n`)
    const seen = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const waitingOn = yield* Deferred.make<string>()
      const watch = (who: string | undefined) => (who === undefined ? Effect.void : Effect.asVoid(Deferred.succeed(waitingOn, who)))
      const fiber = yield* Effect.forkChild(Effect.uninterruptibleMask((restore) =>
        JobCollectors.around({ job, dir: path.join(TREE, `job${job}`), lanes: [LANE], cells: () => [], restore, watch }, Effect.void)
      ))
      const who = yield* Deferred.await(waitingOn)
      const interrupted = Date.now()
      yield* Fiber.interrupt(fiber)
      return { who, tookMs: Date.now() - interrupted }
    }).pipe(Effect.provide(roots)))
    expect(seen.who).toContain("another job's collector")
    expect(seen.tookMs).toBeLessThan(5000)
    expect(lab.trace()).toEqual([])
    expect(fs.readFileSync(holder, "utf8")).toContain("another job's collector")
  }, 20_000)

  it("hand the caller what they answered of each cell where it lands its cells itself, and tell it who left the lab changed", async () => {
    const lab = await labOf({
      collect: `echo '{"verification":"lab-check","cell":"c1","answer":"fail","message":"the lab saw it"}'`,
      close: "exit 1"
    })
    const seen = await withStore(lab.layer, Effect.gen(function*() {
      const job = yield* jobOf
      const cell = yield* landedCell
      let answered: Array<[string, unknown]> = []
      let left: ReadonlyArray<string> = []
      yield* JobCollectors.around({
        job,
        dir: path.join(TREE, `job${job}`),
        lanes: [LANE],
        cells: () => [cell],
        answered: (cells, collected) =>
          Effect.sync(() => {
            answered = cells.map((one) => [one.id, collected.map((by) => by.cells.get(one.id)?.verifications)])
          }),
        leftChanged: (labels) => Effect.sync(() => void (left = labels))
      }, Effect.void)
      const lines = yield* Effect.flatMap(Verifications.Service, (store) => store.of({ scope: "cell", runId: cell.runId, caseId: cell.caseId }))
      return { answered, left, lines }
    }))
    expect(seen.answered).toEqual([["c1", [[{ name: "lab-check", source: "lab", answer: "fail", message: "the lab saw it" }]]]])
    expect(seen.lines).toEqual([{ name: "call", source: "interpreter", answer: "pass" }])
    expect(seen.left).toEqual(["lab"])
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
