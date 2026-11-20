/**
 * The reference campaign: the reference pack's two cases run by the base's
 * campaign run on its simulated lane, each cell one process of the plain
 * replay binary on a paused clock. Both pass; each cell's call record is the
 * one its replay exported, joined to it on its caller's Call-ID and judged by
 * the pack's `call-records` judge, which attaches it flattened and its claimed
 * fields compared; the run is recorded in the store as the `reference`
 * campaign. The operations a run names as its setup are injected before its
 * cells and healed after them, each phase an event of its job; one needing a
 * part no lane of the run names refuses the run before its job opens. On the
 * reference lane, through a workspace holding it on free loopback addresses,
 * the base stands the upstream B2BUA runner for the batch and runs both cells
 * in the plain binary's batch process on the real clock (about 0.8 s); both
 * pass, in either order of the cases, `calls-ended` reading none of each
 * cell's calls among the runner's live ones after it, the batch's end reads
 * the runner holding none, the `call-records` collector reads the record the
 * runner wrote of each call, which the judge passes, and the runner captured
 * both legs of each call while `capture`, cutting for failed cells alone,
 * attaches nothing. The runner's log, capture and record files hold no
 * earlier job's lines, and `sut-log` attaches to the batch the lines the
 * runner wrote over it, from its first call on, none written before. A job
 * collector of the workspace failing `bob is busy` has `capture` attach that
 * cell, and it alone, its calls' packets.
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 *
 * The replay binary is `REPLAY_BIN`, else the workspace's build of
 * `pivot-replay` (`cargo build -p pivot-replay`); the runner's is
 * `B2BUA_RUNNER_BIN`, else the build of `b2bua-runner`; the lint's is
 * `PIVOT_SCHEMA_BIN`, else the release build of `pivot-schema`. A missing
 * one FAILS, naming its variable; `TOOLCHAIN_TEST_SKIP=1` skips the suite
 * instead.
 */
import { NodeServices } from "@effect/platform-node"
import { CellOfBundle } from "@sip/rig"
import { BaseCampaign, manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignResults, CampaignRuns, JobEvents, Jobs } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { blocksOf, callIdOf, payloadOf } from "../collectors/capture/cut.ts"
import { hostedWorkspace, writtenUnder } from "./hosted-workspace.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")

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

const SCRATCH = await scratch("reference_campaign_test")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "reference-campaign-"))

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
 * The reference campaign run once on `lane` as the job it creates, its lanes
 * the workspace's where one is named, the operations `setup` names as its
 * setup, its cases those of `sources` where named, else the pack's: its exit
 * code, the run the store holds of it, its job's events, its directory and
 * its job.
 */
const reference = (lane: string, workspace?: string, setup: ReadonlyArray<string> = [], sources?: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const manifest = yield* manifestOf(PACK)
    const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
      env: "reference-test",
      checkout: CHECKOUT,
      pack: { dir: PACK, manifest }
    })
    const out = path.join(work, `run-${lane}`)
    return yield* Effect.gen(function*() {
      const job = yield* (yield* Jobs.Service).create({ kind: "campaign", args: {}, createdBy: "reference-test" })
      const code = yield* BaseCampaign.run({
        pack: PACK,
        workspace,
        lanes: lane,
        out,
        runs: work,
        junit: undefined,
        job,
        env: "reference-test",
        site: "http://127.0.0.1:5175",
        replay: REPLAY,
        runner: RUNNER,
        pivotSchema: PIVOT_SCHEMA,
        collection: undefined,
        setup,
        sources
      })
      const runs = yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })
      const events = yield* (yield* JobEvents.Service).ofJob(job)
      return { code, run: runs[0], events, out, job }
    }).pipe(Effect.provide(services))
  }).pipe(Effect.provide(NodeServices.layer))

/**
 * The workspace `dir` holding `note`, an operation needing no part whose one
 * executable says the step it runs as, in its step's log.
 */
const notingWorkspace = (dir: string): string => {
  const operation = path.join(dir, "operations", "note")
  fs.mkdirSync(operation, { recursive: true })
  fs.writeFileSync(
    path.join(operation, "operation.toml"),
    [`schema = "operation.v1"`, `name = "note"`, `[run]`, `inject = "./run"`, `heal = "./run"`, ``].join("\n")
  )
  fs.writeFileSync(path.join(operation, "run"), `#!/bin/sh\necho "noted $SIPRIG_STEP" >&2\n`, { mode: 0o755 })
  return dir
}

/** The Call-IDs of the legs of the cell of `caseId` whose bundle is `bundle`, as the base describes the cell to its collectors, sorted. */
const legCallIdsOf = (bundle: string, caseId: string): Promise<ReadonlyArray<string>> =>
  Effect.runPromise(
    CellOfBundle.ofBundle(bundle, { id: "cell", case: caseId, lane: "reference", batch: "batch", attempt: 1 }).pipe(
      Effect.map((cell) => cell.legs.flatMap((leg) => (leg.call_id === undefined ? [] : [leg.call_id])).sort()),
      Effect.provide(NodeServices.layer)
    )
  )

/** The Call-IDs of the SIP messages a pcapng capture holds, each once, in order. */
const callIdsIn = (capture: Uint8Array): ReadonlyArray<string> => [
  ...new Set(blocksOf(capture).filter((block) => block.type === 6).flatMap((block) => callIdOf(payloadOf(block)) ?? []))
]

/** The JSON lines of `file`. */
const linesOf = (file: string): ReadonlyArray<Record<string, unknown>> =>
  fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as Record<string, unknown>)

/**
 * The directory job `job`'s one batch attaches in, under the job's own in the
 * session tree, and the lines of its `attachments.jsonl`.
 */
const batchAttachmentsOf = (job: number): { readonly dir: string; readonly lines: ReadonlyArray<Record<string, unknown>> } => {
  const batches = path.join(work, "sessions", "_jobs", String(job), "batches")
  const listed = (fs.readdirSync(batches, { recursive: true }) as Array<string>).filter((file) => path.basename(file) === "attachments.jsonl")
  expect(listed, "one batch").toHaveLength(1)
  const dir = path.join(batches, path.dirname(listed[0]!))
  return { dir, lines: linesOf(path.join(dir, "attachments.jsonl")) }
}

/** What an earlier job left in the runner's log, capture and record files: a Call-ID no cell holds. */
const STALE = "stale-call@192.0.2.1"

/** The block type of a pcapng section header, read little-endian. */
const SECTION_HEADER = 0x0a0d0d0a

/**
 * The `call-records` judge's attachments to the cell of `bundle`, listed in
 * `attachments`: `record`, the one joined to it, flattened (`records`), and
 * its claimed fields against it, none differing (`diff`).
 */
const expectJudged = (bundle: string, attachments: ReadonlyArray<Record<string, unknown>>, record: Record<string, unknown>) => {
  const named = (name: string) => attachments.filter((line) => line["name"] === name).at(-1)
  const records = named("call-records")
  expect([records?.["kind"], records?.["source"]]).toEqual(["records", "call-records"])
  const flat = linesOf(path.join(bundle, String(records!["file"])))
  expect(flat.map((one) => one["a_leg.call_id"])).toEqual([(record["a_leg"] as { call_id: string }).call_id])
  const diff = named("call-records-diff")
  expect([diff?.["kind"], diff?.["source"]]).toEqual(["diff", "call-records"])
  const compared = JSON.parse(fs.readFileSync(path.join(bundle, String(diff!["file"])), "utf8")) as { rows: unknown; same: number }
  expect(compared.rows, "every claimed field as the record holds it").toEqual([])
  expect(compared.same).toBeGreaterThan(1)
}

/**
 * The `calls-ended` collector's attachments to the cell of `bundle`, listed
 * in `attachments`: the runner's live calls as it read them, none of them the
 * cell's (`json`), and where it read them (`link`).
 */
const expectEnded = (bundle: string, attachments: ReadonlyArray<Record<string, unknown>>) => {
  const reading = attachments.filter((line) => line["source"] === "calls-ended")
  expect(reading.map((line) => [line["name"], line["kind"]])).toEqual([["calls-ended", "json"], ["calls-ended-calls", "link"]])
  const read = JSON.parse(fs.readFileSync(path.join(bundle, String(reading[0]!["file"])), "utf8")) as { held: unknown }
  expect(read.held, "none of the cell's calls is live").toEqual([])
  const where = new URL(String(reading[1]!["file"]))
  expect([where.protocol, where.hostname, where.pathname], "where the calls were read").toEqual(["http:", "127.0.0.1", "/admin/calls"])
}

/** A case source under `dir` holding the pack's cases in the reverse order. */
const reversedSource = (dir: string): string => {
  fs.mkdirSync(dir, { recursive: true })
  const source = path.join(dir, "reversed.ts")
  fs.writeFileSync(
    source,
    `import cases from ${JSON.stringify(path.join(PACK, "cases.ts"))}\nexport default [...cases].reverse()\n`
  )
  return source
}

describe.skipIf(!served || skipping)("the reference campaign", () => {
  it("passes both cases on the simulated lane, each call record exported, joined on its Call-ID and judged", async () => {
    const { code, run } = await Effect.runPromise(reference("simulated"))
    expect(code).toBe(0)
    expect(run?.lanes.map((lane) => [lane.lane, lane.status])).toEqual([["simulated", "done"]])
    const results = run!.lanes[0]!.results
    expect(results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed])).toEqual([
      ["alice calls bob", true],
      ["bob is busy", true]
    ])
    for (const result of results) {
      const judged = result.verifications.filter((line) => line.name === "call-records")
      expect(judged.map((line) => [line.source, line.answer, line.message])).toEqual([["call-records", "pass", "1 record(s), 1 claimed"]])
      const bundle = path.join(run!.lanes[0]!.bundlePath, result.cell!)
      const exported = linesOf(path.join(bundle, "exports", "call-records.jsonl"))
      const joined = linesOf(path.join(bundle, "joined", "call-records.jsonl"))
      expect(exported.length).toBe(1)
      expect(joined).toEqual([exported[0]!["record"]])
      expectJudged(bundle, linesOf(path.join(bundle, "attachments.jsonl")), joined[0]!)
    }
  })

  it("injects the setup operations it is given before its cells and heals them after, each phase an event of its job", async () => {
    const { code, run, events, out } = await Effect.runPromise(reference("simulated", notingWorkspace(path.join(work, "noting")), ["note"]))
    expect(code).toBe(0)
    expect(run?.lanes.map((lane) => [lane.lane, lane.status])).toEqual([["simulated", "done"]])
    expect(events.filter((event) => event.subject === "note").map((event) => event.phase)).toEqual([
      "inject",
      "injected",
      "heal",
      "healed",
      "recovered"
    ])
    const ran = path.join(out, "operations", "1-note")
    expect(fs.readFileSync(path.join(ran, "inject.log"), "utf8")).toContain("noted inject")
    expect(fs.readFileSync(path.join(ran, "heal.log"), "utf8")).toContain("noted heal")
  })

  it("refuses, before its job opens, a setup operation needing a part no lane of the run names", async () => {
    const { code, events } = await Effect.runPromise(reference("simulated", undefined, ["restart-worker"]))
    expect(code).toBe(2)
    expect(events).toEqual([])
  })

  it("passes both cases on the reference lane, the B2BUA runner stood for the batch and holding no call at its end", async () => {
    fs.mkdirSync(path.join(work, "passing"), { recursive: true })
    const written = writtenUnder(path.join(work, "passing"), "b2bua-runner")
    fs.writeFileSync(written.log, `${STALE}\n`)
    fs.writeFileSync(written.capture, STALE)
    fs.writeFileSync(written.cdr, `${JSON.stringify({ a_leg: { call_id: STALE } })}\n`)
    const workspace = await hostedWorkspace(PACK, path.join(work, "workspace"), written)
    const { code, run, job } = await Effect.runPromise(reference("reference", workspace))
    expect(code).toBe(0)
    expect(run?.lanes.map((lane) => [lane.lane, lane.status])).toEqual([["reference", "done"]])
    const results = run!.lanes[0]!.results
    expect(results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed])).toEqual([
      ["alice calls bob", true],
      ["bob is busy", true]
    ])
    const capture = fs.readFileSync(written.capture)
    expect(capture.readUInt32LE(0), "the job's capture alone, from its first section header").toBe(SECTION_HEADER)
    expect(capture.includes(STALE), "what an earlier job left is gone").toBe(false)
    const captured = callIdsIn(capture)
    const recorded = linesOf(written.cdr).map((record) => (record["a_leg"] as { call_id: string }).call_id)
    expect(recorded, "the job's records alone, one per call").toHaveLength(2)
    for (const result of results) {
      const answered = Object.fromEntries(result.verifications.map((line) => [line.name, [line.source, line.answer]]))
      expect(answered["call"]?.[1]).toBe("pass")
      expect(answered["cleanup"]?.[1]).toBe("pass")
      expect(answered["calls-ended"], "read after the call, the worker holding no call").toEqual(["calls-ended", "pass"])
      expect(answered["call-records"], "the runner's record of the call, read by the collector, joined and judged").toEqual([
        "call-records",
        "pass"
      ])
      const bundle = path.join(run!.lanes[0]!.bundlePath, result.cell!)
      const attachments = linesOf(path.join(bundle, "attachments.jsonl"))
      const joined = linesOf(path.join(bundle, "joined", "call-records.jsonl"))
      expect(joined).toHaveLength(1)
      expect(recorded, "the runner wrote the record joined to the cell").toContain((joined[0]!["a_leg"] as { call_id: string }).call_id)
      expectJudged(bundle, attachments, joined[0]!)
      const attached = attachments.filter((line) => line["name"] === "sut-log")
      expect(attached.map((line) => [line["kind"], line["source"]])).toEqual([["log", "sut-log"]])
      expect(fs.readFileSync(path.join(bundle, String(attached[0]!["file"])), "utf8"), "cut to the call's window, the runner's start before it").not.toContain("listening UDP")
      expect(attachments.filter((line) => line["source"] === "capture"), "no cell failed: the capture is not cut").toEqual([])
      expectEnded(bundle, attachments)
      const legs = await legCallIdsOf(bundle, result.caseId)
      expect(legs, "the caller's Call-ID and the runner's b-leg one").toHaveLength(2)
      expect(captured, "the runner captured both legs of the call").toEqual(expect.arrayContaining([...legs]))
    }
    expect(fs.readFileSync(written.log, "utf8"), "what earlier batches left in the runner's log is gone").not.toContain(STALE)
    const batch = batchAttachmentsOf(job)
    const logged = batch.lines.filter((line) => line["source"] === "sut-log")
    expect(logged.map((line) => line["kind"])).toEqual(["log"])
    const bracketed = fs.readFileSync(path.join(batch.dir, String(logged[0]!["file"])), "utf8")
    expect(bracketed, "the line the runner wrote at the batch's first call").toContain("readiness transition")
    expect(bracketed, "nothing written before the batch's collectors opened").not.toContain(STALE)
  })

  it("passes both cases in the reverse order on the reference lane, calls-ended reading each cell's own calls ended", async () => {
    const written = writtenUnder(path.join(work, "reversed"), "b2bua-runner")
    const workspace = await hostedWorkspace(PACK, path.join(work, "reversed-workspace"), written)
    const { code, run } = await Effect.runPromise(
      reference("reference", workspace, [], [reversedSource(path.join(work, "reversed-cases"))])
    )
    expect(code).toBe(0)
    const results = run!.lanes[0]!.results
    expect(results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed]).sort()).toEqual([
      ["alice calls bob", true],
      ["bob is busy", true]
    ])
    for (const result of results) {
      const answered = result.verifications.filter((line) => line.name === "calls-ended")
      expect(answered.map((line) => [line.source, line.answer])).toEqual([["calls-ended", "pass"]])
      const bundle = path.join(run!.lanes[0]!.bundlePath, result.cell!)
      expectEnded(bundle, linesOf(path.join(bundle, "attachments.jsonl")))
    }
  })

  it("cuts the runner's capture for the failed cell alone, its calls' packets a pcapng of their own", async () => {
    const written = writtenUnder(path.join(work, "failing"), "b2bua-runner")
    const workspace = await hostedWorkspace(PACK, path.join(work, "failing-workspace"), written, true)
    const { run } = await Effect.runPromise(reference("reference", workspace))
    const results = run!.lanes[0]!.results
    expect(results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed])).toEqual([
      ["alice calls bob", true],
      ["bob is busy", false]
    ])
    for (const result of results) {
      const bundle = path.join(run!.lanes[0]!.bundlePath, result.cell!)
      const cut = linesOf(path.join(bundle, "attachments.jsonl")).filter((line) => line["source"] === "capture")
      if (result.passed) {
        expect(cut).toEqual([])
        continue
      }
      expect(cut.map((line) => line["kind"])).toEqual(["file", "table"])
      expect(cut[1]!["name"]).toBe("capture-packets")
      const packets = fs.readFileSync(path.join(bundle, String(cut[0]!["file"])))
      const ids = callIdsIn(packets)
      expect([...ids].sort(), "both legs of the busy call, and no other call").toEqual(await legCallIdsOf(bundle, result.caseId))
      const [columns, ...rows] = fs.readFileSync(path.join(bundle, String(cut[1]!["file"])), "utf8").split("\n")
        .filter((line) => line !== "").map((line) => JSON.parse(line) as ReadonlyArray<string>)
      expect(columns).toEqual(["packet", "start line", "Call-ID"])
      expect(rows, "one row per packet of the cut").toHaveLength(blocksOf(packets).filter((block) => block.type === 6).length)
      expect(rows.map((row) => row[1])).toEqual(expect.arrayContaining([
        expect.stringMatching(/^INVITE sip:/),
        expect.stringMatching(/^SIP\/2\.0 486 /)
      ]))
    }
  })
})
