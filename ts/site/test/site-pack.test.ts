/**
 * The site reads its deployment through the site pack and names nothing of
 * its own: the config's bind address, public URL, port and link base are read
 * by the pack's variables, an unstated triage lane and source command are the
 * pack's, a lane's SUT line is the pack's for the simulated lane and the SIP
 * target's description for a hosted one, and an HTTP exchange's answer reads
 * as the pack reads it.
 */
import { NodeServices } from "@effect/platform-node"
import { Effect, Layer } from "effect"
import * as ConfigProvider from "effect/ConfigProvider"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { routingLine } from "../src/cell-digest.ts"
import { ViewerConfig } from "../src/config.ts"
import { laneRoles, sutOf } from "../src/lane-roles.ts"
import { SitePack } from "../src/site-pack.ts"
import { PACK } from "./fixture-pack.ts"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "site-pack-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The config a file stating a triage block loads to under `env`, over {@link PACK}. */
const load = (env: Record<string, string>) => {
  const file = path.join(dir, "site.config.json")
  fs.writeFileSync(
    file,
    JSON.stringify({ caseRoots: [], triage: { registry: "registry.json", repoRoot: dir, corpus: "/corpus", sessions: "/sessions" } })
  )
  return Effect.runPromise(
    Effect.flatMap(ViewerConfig.Service, Effect.succeed).pipe(
      Effect.provide(
        ViewerConfig.layer.pipe(
          Layer.provide(Layer.succeed(SitePack.Service, PACK)),
          Layer.provide(NodeServices.layer),
          Layer.provide(ConfigProvider.layer(ConfigProvider.fromUnknown({ SIPRIG_SITE_CONFIG: file, ...env })))
        )
      )
    )
  )
}

describe("the config over a site pack", () => {
  it("reads the bind address, public URL and link base by the pack's variables, the port its default", async () => {
    const values = await load({ RIG_BIND: "0.0.0.0", RIG_PUBLIC_URL: "https://site.notreal", RIG_LINK_BASE: "http://links.notreal/" })
    expect(values.bind).toBe("0.0.0.0")
    expect(values.publicUrl).toBe("https://site.notreal")
    expect(values.port).toBe(6123)
    expect(values.linkBase).toBe("http://links.notreal")
  })

  it("takes the port the pack's port variable states", async () => {
    expect((await load({ RIG_PORT: "6200" })).port).toBe(6200)
  })

  it("reports on the pack's default lane and runs its source command where the triage block states neither", async () => {
    const triage = (await load({})).triage
    expect(triage?.lane).toBe("sim")
    expect(triage?.ingest).toEqual({ command: "rig-ingest", cwd: dir })
  })
})

describe("a cell's HTTP exchange", () => {
  const exchange = { ordinal: 2, atMs: 15, method: "POST", path: "/decide", status: 200 }

  it("reads in a line: what was asked and answered, then the pack's parts of its answer body", () => {
    expect(routingLine({ ...exchange, responseBody: { verdict: "go", opaque: "x" } }, PACK.routingLineOf))
      .toBe("http#2 +15ms POST /decide → 200 · said go")
  })

  it("is the exchange alone where the pack reads nothing off its answer, and names a missing answer", () => {
    expect(routingLine({ ...exchange, responseBody: { opaque: "x" } }, PACK.routingLineOf)).toBe("http#2 +15ms POST /decide → 200")
    const { status: _, ...unanswered } = exchange
    expect(routingLine({ ...unanswered, error: "timeout", responseBody: undefined }, PACK.routingLineOf))
      .toBe("http#2 +15ms POST /decide → no answer: timeout")
  })
})

describe("a lane's SUT", () => {
  it("is the pack's line on the simulated lane and the SIP target's description on a hosted one", () => {
    expect(laneRoles(PACK)).toEqual([
      ["sim", PACK.simulatedSut],
      ["hosted", "a B2BUA on a test bench"],
      ["dark", "a B2BUA with no harness on its routing path"]
    ])
    expect(sutOf(PACK, "elsewhere")).toBe("a lane this deployment does not describe")
  })
})
