/**
 * A workspace is placed by the tooling's facts: its variable names it, its
 * file and its own directories stand beside the generic ones, and the
 * environment it states is in the tooling's variables; applied, an exported
 * variable wins, and a workspace without its file is refused naming the
 * command that writes one.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as ConfigFile from "../src/config-file.js"
import * as Layout from "../src/layout.js"
import * as WorkspaceEnv from "../src/workspace-env.js"
import { PACK } from "./pack.js"

let ws: string
beforeEach(() => {
  ws = fs.mkdtempSync(path.join(os.tmpdir(), "workspace-layout-"))
})
afterEach(() => fs.rmSync(ws, { recursive: true, force: true }))

const run = <A, E>(effect: Effect.Effect<A, E, NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const { decodeText } = ConfigFile.make(PACK, {
  fields: {},
  campaignFields: { coverage: Schema.optionalKey(Schema.Boolean) },
  check: () => undefined
})

const Env = WorkspaceEnv.make(PACK, decodeText)

describe("a workspace's layout", () => {
  it("places the file, the generic directories and the tooling's own under its root", () => {
    const layout = Layout.layoutOf(PACK, "ws", "/srv")
    expect(layout.root).toBe("/srv/ws")
    expect(layout.config).toBe("/srv/ws/tool.toml")
    expect(layout.seeds).toBe("/srv/ws/.seeds")
    expect(layout.runs).toBe("/srv/ws/runs")
    expect(layout.extras).toBe("/srv/ws/extras")
  })

  it("is named by the tooling's variable alone", () => {
    expect(Layout.of(PACK, { TOOL_WORKSPACE: "/srv/ws" })?.root).toBe("/srv/ws")
    expect(Layout.of(PACK, { TOOL_WORKSPACE: "  " })).toBeUndefined()
    expect(Layout.of(PACK, { OTHER_WORKSPACE: "/srv/ws" })).toBeUndefined()
  })

  it("states its roots and its file's keys in the tooling's variables, the store its own postgres on the pack's port by default", () => {
    const layout = Layout.layoutOf(PACK, "/srv/ws")
    expect(Layout.defaultsOf(PACK, layout, {})).toEqual({
      TOOL_ARTIFACTS: "/srv/ws/artifacts",
      TOOL_SESSIONS: "/srv/ws/runs",
      TOOL_CENSUS: "/srv/ws/census",
      TOOL_PCAPS: "/srv/ws/pcaps",
      TOOL_PCAP_ROOTS: "",
      TOOL_LOCKS: "/srv/ws/locks",
      DATABASE_URL: "postgresql://127.0.0.1:5601/tool_store"
    })
    const stated = Layout.defaultsOf(PACK, layout, {
      site: { port: 6000, bind: "0.0.0.0", public_url: "http://tools.notreal" },
      database: { url: "postgresql://db.notreal/store" },
      tools: { bin_dir: "bin" }
    })
    expect(stated).toMatchObject({
      TOOL_PORT: "6000",
      TOOL_BIND: "0.0.0.0",
      TOOL_PUBLIC_URL: "http://tools.notreal",
      TOOL_BIN_DIR: "/srv/ws/bin",
      DATABASE_URL: "postgresql://db.notreal/store"
    })
  })
})

describe("a workspace applied to an environment", () => {
  it("sets what the environment states not, an exported variable winning", async () => {
    fs.writeFileSync(path.join(ws, "tool.toml"), "[site]\nport = 6000\n")
    const env: NodeJS.ProcessEnv = { TOOL_WORKSPACE: ws, TOOL_LOCKS: "/elsewhere" }
    const applied = await run(Env.apply(env))
    expect(applied).toEqual([{ file: path.join(ws, "tool.toml"), set: expect.any(Array), ignored: ["TOOL_LOCKS"] }])
    expect(env.TOOL_LOCKS).toBe("/elsewhere")
    expect(env.TOOL_PORT).toBe("6000")
    expect(env.TOOL_SESSIONS).toBe(path.join(ws, "runs"))
  })

  it("applies nothing where no workspace is named", async () => {
    const env: NodeJS.ProcessEnv = {}
    expect(await run(Env.apply(env))).toEqual([])
    expect(env).toEqual({})
  })

  it("is refused without its file, naming the command that writes one", async () => {
    const refused = await run(Effect.flip(Env.apply({ TOOL_WORKSPACE: ws })))
    expect(refused.reason).toBe(
      `TOOL_WORKSPACE=${ws} holds no readable tool.toml (${path.join(ws, "tool.toml")}): \`tool-workspace init ${ws}\` writes one`
    )
  })

  it("is refused with a file the schema refuses, naming the file and the key", async () => {
    fs.writeFileSync(path.join(ws, "tool.toml"), "[site]\nport = 0\n")
    const refused = await run(Effect.flip(Env.apply({ TOOL_WORKSPACE: ws })))
    expect(refused.reason).toMatch(new RegExp(`^${path.join(ws, "tool.toml")}: site\\.port: `))
  })
})
