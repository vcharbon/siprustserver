/**
 * An operation as a step of a case (ADR-0060 appendix §7) on the reference
 * lane, run by the base's campaign run over the pack's step source
 * (`step-cases.ts`, named by `sources`), through a workspace declaring its tag
 * `alone`. Once Bob received the ACK, the lane's `restart-worker` SIGKILLs the
 * runner mid-call; the base stands the next runner, which answers each
 * party's BYE 481, and the cell passes, no call record written. The cell's end
 * heals the step and its probe proves the runner gone, each phase an event of
 * the job; the next cell runs on the runner stood again and passes, its call
 * record written. A case asking for a step whose tags state no alone is
 * refused at compile, no call placed.
 *
 * Its own real-clock waits, about 0.9 s: each call's answer, ring and hold
 * (0.26 s) and its `calls-ended` read 0.1 s after it, for two cells, then the
 * `call-records` settle the workspace states ({@link SETTLE_S}), waited in
 * full since the killed call has no record; the rest is process work (the
 * runner's restart, the operation's node processes). The suite builds and
 * drops its OWN database
 * (`@sip/store/test-server`). The binaries are those of
 * `reference-campaign.test.ts`; `TOOLCHAIN_TEST_SKIP=1` skips the suite.
 */
import { NodeServices } from "@effect/platform-node"
import { BaseCampaign, manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignResults, CampaignRuns, JobEvents, Jobs } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { TAG } from "../step-cases.ts"
import { hostedWorkspace, writtenUnder } from "./hosted-workspace.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const STEP_CASES = path.join(PACK, "step-cases.ts")

const binary = (variable: string, fallback: string): string => process.env[variable] ?? path.join(CHECKOUT, fallback)
const REPLAY = binary("REPLAY_BIN", "target/debug/pivot-replay")
const RUNNER = binary("B2BUA_RUNNER_BIN", "target/debug/b2bua-runner")
const PIVOT_SCHEMA = binary("PIVOT_SCHEMA_BIN", "target/release/pivot-schema")

const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
for (
  const [variable, file] of [["REPLAY_BIN", REPLAY], ["B2BUA_RUNNER_BIN", RUNNER], ["PIVOT_SCHEMA_BIN", PIVOT_SCHEMA]] as const
) {
  if (!skipping && !fs.existsSync(file)) {
    throw new Error(`no binary at ${file}: build it, name one with ${variable}, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1`)
  }
}

const SCRATCH = await scratch("step_campaign_test")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "step-campaign-"))

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
}, 120_000)

/**
 * How long `call-records` waits for a record of each cell's call. The killed
 * call never gets one, so the collect waits it in full. The runner writes a
 * call's record through its CDR queue as the call ends, and the collect starts
 * only once the batch's end gate read the runner holding no call, the
 * `calls-ended` read 0.1 s after the last call before it: a record the
 * runner writes reaches the file well within 0.2 s of that.
 */
const SETTLE_S = 0.2

/** The workspace `name` holding the reference lane, its `rig.toml` declaring {@link TAG} alone or not: its directory and what its runner writes. */
const workspaceOf = async (name: string, alone: boolean) => {
  const dir = path.join(work, name)
  const written = writtenUnder(dir, "b2bua-runner")
  await hostedWorkspace(PACK, dir, written)
  fs.appendFileSync(path.join(dir, "lanes", "reference.toml"), `\n[collectors.call-records]\nsettle_s = ${SETTLE_S}\n`)
  fs.writeFileSync(path.join(dir, "rig.toml"), `[tags.${TAG}]\nlabel = "restarts the system under test"\ndefault = "run"\nalone = ${alone}\n`)
  return { dir, written }
}

/** The base campaign's services of the run `name`, over the suite's database. */
const servicesOf = (name: string) =>
  Layer.unwrap(Effect.map(manifestOf(PACK), (manifest) =>
    layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, `sessions-${name}`), {
      env: "step-test",
      checkout: CHECKOUT,
      pack: { dir: PACK, manifest }
    }))).pipe(Layer.provide(NodeServices.layer))

/** The step source run on the reference lane of `workspace` as the job it creates: its exit code, its run, its job's events. */
const stepCampaign = (workspace: string, name: string) =>
  Effect.gen(function*() {
    const job = yield* (yield* Jobs.Service).create({ kind: "campaign", args: {}, createdBy: "step-test" })
    const code = yield* BaseCampaign.run({
      pack: PACK,
      workspace,
      lanes: "reference",
      out: path.join(work, `run-${name}`),
      runs: work,
      junit: undefined,
      job,
      env: "step-test",
      site: "http://127.0.0.1:5175",
      replay: REPLAY,
      runner: RUNNER,
      pivotSchema: PIVOT_SCHEMA,
      collection: undefined,
      setup: [],
      sources: [STEP_CASES]
    })
    const runs = yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })
    const events = yield* (yield* JobEvents.Service).ofJob(job)
    return { code, run: runs[0], events, job }
  }).pipe(Effect.provide(servicesOf(name)), Effect.provide(NodeServices.layer))

/** The JSON lines of `file`. */
const linesOf = (file: string): ReadonlyArray<Record<string, unknown>> =>
  fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as Record<string, unknown>)

/** What the runner wrote over job `job`'s one batch of the run `name`, as `sut-log` attached it to the batch. */
const batchLogOf = (job: number, name: string): string => {
  const batches = path.join(work, `sessions-${name}`, "_jobs", String(job), "batches")
  const listed = (fs.readdirSync(batches, { recursive: true }) as Array<string>).filter((file) => path.basename(file) === "attachments.jsonl")
  expect(listed, "one batch").toHaveLength(1)
  const dir = path.join(batches, path.dirname(listed[0]!))
  const logged = linesOf(path.join(dir, "attachments.jsonl")).filter((line) => line["source"] === "sut-log")
  expect(logged.map((line) => line["kind"])).toEqual(["log"])
  return fs.readFileSync(path.join(dir, String(logged[0]!["file"])), "utf8")
}

describe.skipIf(!served || skipping)("an operation as a step of a case on the reference lane", () => {
  it("kills the runner mid-call, the call ending on the next runner's 481s, heals, and runs the next cell on the runner stood again", async () => {
    const { dir, written } = await workspaceOf("alone", true)
    const { code, run, events, job } = await Effect.runPromise(stepCampaign(dir, "alone"))
    expect(code).toBe(0)
    expect(run?.lanes.map((lane) => [lane.lane, lane.status])).toEqual([["reference", "done"]])
    const results = new Map(run!.lanes[0]!.results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result]))
    const killed = results.get("alice calls bob through a restart")
    const after = results.get("alice calls bob after the restart")
    expect([killed?.passed, after?.passed]).toEqual([true, true])
    const answered = (result: typeof killed) => Object.fromEntries(result!.verifications.map((line) => [line.name, line.answer]))
    expect(answered(killed)["call"]).toBe("pass")
    expect(answered(killed)["cleanup"]).toBe("pass")
    expect(answered(killed)["call-records"], "no record written of the call the runner was killed in").toBe("pass")
    expect(answered(after)["call-records"], "the record the runner stood again wrote of the next call").toBe("pass")
    const step = events.filter((event) => event.subject === `restart-worker for ${killed!.cell}`)
    expect(step.map((event) => event.phase)).toEqual(["inject", "injected", "heal", "healed", "recovered"])
    expect(events.filter((event) => event.phase === JobEvents.LAB_LEFT_CHANGED)).toEqual([])
    const log = batchLogOf(job, "alone")
    expect(log.split("listening UDP").length - 1, "the runner stood again after the kill, within the batch").toBe(1)
    expect(linesOf(written.cdr), "one record: the call after the restart, which the killed runner could not write").toHaveLength(1)
  })

  it("runs no later cell of the batch once a step left the lab changed, each an environment failure, and closes the job failed", async () => {
    const { dir } = await workspaceOf("left-changed", true)
    const operations = path.join(PACK, "operations")
    const stuck = path.join(dir, "operations", "stuck-restart")
    fs.mkdirSync(stuck, { recursive: true })
    for (const file of ["main.ts", "run"]) fs.copyFileSync(path.join(operations, "restart-worker", file), path.join(stuck, file))
    fs.chmodSync(path.join(stuck, "run"), 0o755)
    fs.writeFileSync(path.join(stuck, "never"), "#!/bin/sh\necho 'this heal never heals' >&2\nexit 1\n", { mode: 0o755 })
    fs.writeFileSync(
      path.join(stuck, "operation.toml"),
      fs.readFileSync(path.join(operations, "restart-worker", "operation.toml"), "utf8")
        .replace(`name = "restart-worker"`, `name = "stuck-restart"`)
        .replace(`heal = "./run"`, `heal = "./never"`)
    )
    const lane = path.join(dir, "lanes", "reference.toml")
    fs.writeFileSync(lane, fs.readFileSync(lane, "utf8").replace(`operation = "restart-worker"`, `operation = "stuck-restart"`))
    const { run, events, job } = await Effect.runPromise(stepCampaign(dir, "left-changed"))
    const results = new Map(run!.lanes[0]!.results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result]))
    const after = results.get("alice calls bob after the restart")
    expect(after?.passed).toBe(false)
    expect(after?.verifications, "no call placed").toEqual([])
    expect(after?.errorDetail).toContain("not run: the step restart-worker for")
    expect(after?.errorDetail).toContain("left the lab changed")
    expect(events.filter((event) => event.phase === JobEvents.LAB_LEFT_CHANGED)).toHaveLength(1)
    const { row, marked } = await Effect.runPromise(
      Effect.gen(function*() {
        const row = yield* (yield* Jobs.Service).show(job)
        const marked = yield* (yield* JobEvents.Service).labLeftChanged(job)
        return { row, marked }
      }).pipe(Effect.provide(servicesOf("left-changed")))
    )
    expect(marked, "the job marked as having left its lab changed").toBe(true)
    expect(row?.job.status, "the job closed failed").toBe("failed")
  })

  it("refuses at compile a case asking for a step whose tags state no alone, no call placed", async () => {
    const { dir, written } = await workspaceOf("shared", false)
    const { code, run, events } = await Effect.runPromise(stepCampaign(dir, "shared"))
    expect(code).toBe(2)
    expect(JSON.stringify(run?.lanes.map((lane) => [lane.status, lane.detail]))).toContain(
      "injects restart-worker as a step of its call, and carries no tag stating alone"
    )
    expect(events).toEqual([])
    expect(fs.existsSync(written.cdr), "no runner stood, no call placed").toBe(false)
  })
})
