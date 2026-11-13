/**
 * `siprig-campaign run`, the command the base image runs for a pack with no
 * run verb (`@sip/siprig` `campaign-cli.ts`), on the reference pack that
 * `SIPRIG_PACK` names, with no workspace: it runs the pack's simulated lane,
 * exits 0, and the store `DATABASE_URL` names holds the run as the
 * `reference` campaign. A pack directory holding no manifest exits 2 naming
 * it. The suite builds and drops its OWN database (`@sip/store/test-server`).
 *
 * Its binaries are found as `reference-campaign.test.ts` finds them;
 * `TOOLCHAIN_TEST_SKIP=1` skips the suite.
 */
import { NodeServices } from "@effect/platform-node"
import { manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignRuns } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const CLI = path.join(CHECKOUT, "ts", "siprig", "src", "campaign-cli.ts")
const TSX = path.join(CHECKOUT, "ts", "siprig", "node_modules", ".bin", "tsx")

const binary = (variable: string, fallback: string): string => process.env[variable] ?? path.join(CHECKOUT, fallback)

const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
const SCRATCH = await scratch("campaign_cli_test")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "campaign-cli-"))

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
}, 120_000)

/** `siprig-campaign run --lanes simulated --out <run>` with the pack `pack` named; its exit code and its output. */
const campaign = (pack: string) => {
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    SIPRIG_PACK: pack,
    SIPRIG_WORKSPACE: "",
    SIPRIG_ENV: "cli-test",
    SIPRIG_SESSIONS: path.join(work, "sessions"),
    DATABASE_URL: urlFor(SCRATCH),
    REPLAY_BIN: binary("REPLAY_BIN", "target/debug/pivot-replay"),
    PIVOT_SCHEMA_BIN: binary("PIVOT_SCHEMA_BIN", "target/release/pivot-schema")
  }
  const ran = spawnSync(TSX, [CLI, "run", "--lanes", "simulated", "--out", path.join(work, "run")], { env, encoding: "utf8" })
  return { code: ran.status, output: `${ran.stdout}${ran.stderr}` }
}

describe.skipIf(!served || skipping)("siprig-campaign run", () => {
  it("runs the pack SIPRIG_PACK names and records it in the store DATABASE_URL names", async () => {
    const { code, output } = campaign(PACK)
    expect(code, output).toBe(0)
    const read = Effect.gen(function*() {
      const manifest = yield* manifestOf(PACK)
      const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
        env: "cli-test",
        checkout: CHECKOUT,
        pack: { dir: PACK, manifest }
      })
      return yield* Effect.flatMap(CampaignRuns.Service, (store) => store.read("reference", { limit: 1 })).pipe(
        Effect.provide(services)
      )
    })
    const runs = await Effect.runPromise(read.pipe(Effect.provide(NodeServices.layer)))
    expect(runs[0]?.lanes.map((lane) => [lane.lane, lane.status, lane.results.map((result) => result.passed)])).toEqual([
      ["simulated", "done", [true, true]]
    ])
  })

  it("exits 2 naming the manifest a pack directory lacks", () => {
    const { code, output } = campaign(work)
    expect(code).toBe(2)
    expect(output).toContain("pack.toml")
  })
})
