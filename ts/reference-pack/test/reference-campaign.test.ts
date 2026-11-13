/**
 * The reference campaign: the reference pack's two cases run by the base's
 * campaign run on its simulated lane, each cell one process of the plain
 * replay binary on a paused clock. Both pass; each cell's call record is the
 * one its replay exported, joined to it on its caller's Call-ID and judged by
 * the pack's `call-records` judge; the run is recorded in the store as the
 * `reference` campaign. The suite builds and drops its OWN database
 * (`@sip/store/test-server`).
 *
 * The replay binary is `REPLAY_BIN`, else the workspace's build of
 * `pivot-replay` (`cargo build -p pivot-replay`); the lint's is
 * `PIVOT_SCHEMA_BIN`, else the release build of `pivot-schema`. A missing
 * one FAILS, naming its variable; `TOOLCHAIN_TEST_SKIP=1` skips the suite
 * instead.
 */
import { NodeServices } from "@effect/platform-node"
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
import { afterAll, beforeAll, describe, expect, it } from "vitest"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")

const binary = (variable: string, fallback: string): string => process.env[variable] ?? path.join(CHECKOUT, fallback)
const REPLAY = binary("REPLAY_BIN", "target/debug/pivot-replay")
const PIVOT_SCHEMA = binary("PIVOT_SCHEMA_BIN", "target/release/pivot-schema")

const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
for (const [variable, file] of [["REPLAY_BIN", REPLAY], ["PIVOT_SCHEMA_BIN", PIVOT_SCHEMA]] as const) {
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

/** The reference campaign run once on the simulated lane: its exit code and the run the store holds of it. */
const reference = Effect.gen(function*() {
  const manifest = yield* manifestOf(PACK)
  const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
    env: "reference-test",
    checkout: CHECKOUT,
    pack: { dir: PACK, manifest }
  })
  return yield* Effect.gen(function*() {
    const code = yield* BaseCampaign.run({
      pack: PACK,
      workspace: undefined,
      lanes: undefined,
      out: path.join(work, "run"),
      runs: work,
      junit: undefined,
      job: undefined,
      env: "reference-test",
      site: "http://127.0.0.1:5175",
      replay: REPLAY,
      pivotSchema: PIVOT_SCHEMA,
      collection: undefined
    })
    const runs = yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })
    return { code, run: runs[0] }
  }).pipe(Effect.provide(services))
}).pipe(Effect.provide(NodeServices.layer))

/** The JSON lines of `file`. */
const linesOf = (file: string): ReadonlyArray<Record<string, unknown>> =>
  fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as Record<string, unknown>)

describe.skipIf(!served || skipping)("the reference campaign", () => {
  it("passes both cases on the simulated lane, each call record exported, joined on its Call-ID and judged", async () => {
    const { code, run } = await Effect.runPromise(reference)
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
    }
  })
})
