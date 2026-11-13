/**
 * The reference campaign: the reference pack's two cases run by the base's
 * campaign run on its simulated lane, each cell one process of the plain
 * replay binary on a paused clock. Both pass; each cell's call record is the
 * one its replay exported, joined to it on its caller's Call-ID and judged by
 * the pack's `call-records` judge, which attaches it flattened and its claimed
 * fields compared; the run is recorded in the store as the `reference`
 * campaign. On the reference lane, through a workspace holding it on free
 * loopback addresses, the base stands the upstream B2BUA runner for the batch
 * and runs both cells in the plain binary's batch process on the real clock
 * (about 0.7 s); both pass, `calls-ended` reads the runner holding no call
 * after each, the batch's end reads it holding none, the `call-records`
 * collector reads the record the runner wrote of each call, which the judge
 * passes, and the runner captured both legs of each call while `capture`,
 * cutting for failed cells alone, attaches nothing. The runner's capture and
 * record files hold that job's alone, what an earlier one left gone. A job
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
import { CampaignResults, CampaignRuns } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import * as dgram from "node:dgram"
import * as net from "node:net"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { blocksOf, callIdOf, payloadOf } from "../collectors/capture/cut.ts"

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

/** The reference campaign run once on `lane`, its lanes the workspace's where one is named: its exit code and the run the store holds of it. */
const reference = (lane: string, workspace?: string) =>
  Effect.gen(function*() {
    const manifest = yield* manifestOf(PACK)
    const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
      env: "reference-test",
      checkout: CHECKOUT,
      pack: { dir: PACK, manifest }
    })
    return yield* Effect.gen(function*() {
      const code = yield* BaseCampaign.run({
        pack: PACK,
        workspace,
        lanes: lane,
        out: path.join(work, `run-${lane}`),
        runs: work,
        junit: undefined,
        job: undefined,
        env: "reference-test",
        site: "http://127.0.0.1:5175",
        replay: REPLAY,
        runner: RUNNER,
        pivotSchema: PIVOT_SCHEMA,
        collection: undefined
      })
      const runs = yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })
      return { code, run: runs[0] }
    }).pipe(Effect.provide(services))
  }).pipe(Effect.provide(NodeServices.layer))

/** A loopback address no socket of `kind` held a moment ago. */
const freeAddress = async (kind: "udp" | "tcp"): Promise<string> => {
  if (kind === "tcp") {
    const server = net.createServer()
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve))
    const { port } = server.address() as net.AddressInfo
    await new Promise((resolve) => server.close(resolve))
    return `127.0.0.1:${port}`
  }
  const socket = dgram.createSocket("udp4")
  await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", resolve))
  const { port } = socket.address()
  await new Promise<void>((resolve) => socket.close(resolve))
  return `127.0.0.1:${port}`
}

/** What a hosted workspace's runner writes: its log, its packet capture and its call records. */
interface Written {
  readonly log: string
  readonly capture: string
  readonly cdr: string
}

/**
 * A job collector failing the cell of `bob is busy` (`fail-busy`), so the
 * job's `on-failure` collectors run over that cell alone.
 */
const FAIL_BUSY = {
  "collector.toml": [
    `schema = "collector.v1"`,
    `name = "fail-busy"`,
    `scope = "job"`,
    `[run]`,
    `collect = "./run"`,
    ``
  ].join("\n"),
  run: [
    `#!/usr/bin/env node`,
    `const fs = require("node:fs")`,
    `if (process.env.SIPRIG_STEP === "collect") {`,
    `  const context = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))`,
    `  for (const cell of context.cells.filter((one) => one.case.includes("busy"))) {`,
    `    console.log(JSON.stringify({ verification: "fail-busy", cell: cell.id, answer: "fail", message: "failed by the test" }))`,
    `  }`,
    `}`,
    ``
  ].join("\n")
}

/**
 * A workspace named `name` holding the pack's reference lane on addresses
 * free on this box, so two runs of this suite never share a port: its
 * runner logging and capturing where `written` says, `calls-ended` read
 * 0.1 s after each call so the batch stays under a second, and the
 * `fail-busy` collector where `failing` is set.
 */
const hostedWorkspace = async (name: string, written: Written, failing = false): Promise<string> => {
  const dir = path.join(work, name)
  const lane = fs.readFileSync(path.join(PACK, "lanes", "reference.toml"), "utf8")
    .replace(`ingress = "127.0.0.1:5160"`, `ingress = "${await freeAddress("udp")}"`) +
    `\n[collectors.calls-ended]\ndelay_s = 0.1\n`
  fs.mkdirSync(path.join(dir, "lanes"), { recursive: true })
  fs.mkdirSync(path.join(dir, "targets", "b2bua-runner"), { recursive: true })
  fs.mkdirSync(path.join(dir, "targets", "sip-capture"), { recursive: true })
  fs.mkdirSync(path.join(dir, "targets", "cdr-file"), { recursive: true })
  fs.writeFileSync(path.join(dir, "lanes", "reference.toml"), lane)
  fs.writeFileSync(path.join(dir, "targets", "cdr-file", "reference.toml"), `file = "${written.cdr}"\n`)
  fs.writeFileSync(
    path.join(dir, "targets", "b2bua-runner", "reference.toml"),
    `metrics = "${await freeAddress("tcp")}"\nadmin = "${await freeAddress("tcp")}"\nlog = "${written.log}"\n`
  )
  fs.writeFileSync(path.join(dir, "targets", "sip-capture", "reference.toml"), `file = "${written.capture}"\n`)
  if (failing) {
    const collector = path.join(dir, "collectors", "fail-busy")
    fs.mkdirSync(collector, { recursive: true })
    fs.writeFileSync(path.join(collector, "collector.toml"), FAIL_BUSY["collector.toml"])
    fs.writeFileSync(path.join(collector, "run"), FAIL_BUSY.run, { mode: 0o755 })
  }
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

/** What an earlier job left in the runner's capture and record files: a Call-ID no cell holds. */
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

  it("passes both cases on the reference lane, the B2BUA runner stood for the batch and holding no call at its end", async () => {
    const written = {
      log: path.join(work, "b2bua-runner.log"),
      capture: path.join(work, "passing", "b2bua-runner.pcapng"),
      cdr: path.join(work, "passing", "b2bua-runner.cdr.jsonl")
    }
    fs.mkdirSync(path.join(work, "passing"), { recursive: true })
    fs.writeFileSync(written.capture, STALE)
    fs.writeFileSync(written.cdr, `${JSON.stringify({ a_leg: { call_id: STALE } })}\n`)
    const { code, run } = await Effect.runPromise(reference("reference", await hostedWorkspace("workspace", written)))
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
      const legs = await legCallIdsOf(bundle, result.caseId)
      expect(legs, "the caller's Call-ID and the runner's b-leg one").toHaveLength(2)
      expect(captured, "the runner captured both legs of the call").toEqual(expect.arrayContaining([...legs]))
    }
    expect(fs.readFileSync(written.log, "utf8")).toContain("listening UDP")
  })

  it("cuts the runner's capture for the failed cell alone, its calls' packets a pcapng of their own", async () => {
    const written = {
      log: path.join(work, "failing.log"),
      capture: path.join(work, "failing", "b2bua-runner.pcapng"),
      cdr: path.join(work, "failing", "b2bua-runner.cdr.jsonl")
    }
    const { run } = await Effect.runPromise(reference("reference", await hostedWorkspace("failing-workspace", written, true)))
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
      expect(cut.map((line) => line["kind"])).toEqual(["file"])
      const ids = callIdsIn(fs.readFileSync(path.join(bundle, String(cut[0]!["file"]))))
      expect([...ids].sort(), "both legs of the busy call, and no other call").toEqual(await legCallIdsOf(bundle, result.caseId))
    }
  })
})
