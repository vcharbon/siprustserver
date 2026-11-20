/**
 * The configuration file reads the generic sections and the tooling's own,
 * refusing a key neither names (naming the tooling's file), a value of the
 * wrong shape, and what the tooling's check refuses after the generic
 * checks; a deployment reads a workspace's file, else the checkout's, the
 * tooling's levels beside its own; its campaigns carry the tooling's fields.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type { FileSystem } from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Campaigns from "../src/campaigns.js"
import * as ConfigFile from "../src/config-file.js"
import * as DeploymentConfig from "../src/deployment-config.js"
import * as Layout from "../src/layout.js"
import * as WorkspaceEnv from "../src/workspace-env.js"
import { PACK } from "./pack.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "workspace-config-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

const run = <A, E>(effect: Effect.Effect<A, E, NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const FIELDS = { extra: Schema.optionalKey(Schema.Struct({ mode: Schema.optionalKey(Schema.Literals(["a", "b"])) })) }
const CAMPAIGN_FIELDS = { coverage: Schema.optionalKey(Schema.Boolean) }

const { parse, decodeText } = ConfigFile.make(PACK, {
  fields: FIELDS,
  campaignFields: CAMPAIGN_FIELDS,
  check: (config) => (config.extra?.mode === "b" ? "extra.mode: b is the tooling's to refuse" : undefined)
})

const reason = (text: string): string | undefined => {
  const parsed = parse("tool.toml", text)
  return Result.isFailure(parsed) ? parsed.failure.reason : undefined
}

describe("the configuration file", () => {
  it("reads the generic sections and the tooling's own", () => {
    const text = "[site]\nport = 6000\n[extra]\nmode = \"a\"\n[campaign.routing]\ncases = [\"x/**\"]\ncoverage = false\n"
    expect(parse("tool.toml", text)).toEqual(Result.succeed({
      site: { port: 6000 },
      extra: { mode: "a" },
      campaign: { routing: { cases: ["x/**"], coverage: false } }
    }))
  })

  it("refuses a key neither names, naming the tooling's file", () => {
    expect(reason("[other]\nx = 1\n")).toBe("other: not a key of tool.toml")
    expect(reason("[campaign.routing]\ncases = [\"x\"]\nbogus = 1\n")).toBe("campaign.routing.bogus: not a key of tool.toml")
  })

  it("refuses a value of the wrong shape, in a generic or a tooling section, naming its key", () => {
    expect(reason("[site]\nport = 0\n")).toMatch(/^site\.port: /)
    expect(reason("[extra]\nmode = \"c\"\n")).toMatch(/^extra\.mode: /)
  })

  it("refuses a public URL naming its key, not the tooling's variable", () => {
    expect(reason("[site]\npublic_url = \"ftp://x.notreal\"\n")).toBe("site.public_url: is not an http(s) URL: ftp://x.notreal")
  })

  it("runs the tooling's check after the generic ones: a file both refuse is refused by the generic check", () => {
    expect(reason("[extra]\nmode = \"b\"\n")).toBe("extra.mode: b is the tooling's to refuse")
    expect(reason("[extra]\nmode = \"b\"\n[tags.Bad]\nlabel = \"x\"\ndefault = \"run\"\n")).toBe(
      "tags.Bad: a tag name is lower-case letters, digits and dashes"
    )
  })

  it("takes no tooling field whose decoding needs a service: the typecheck refuses one, and takes the same field needing none", () => {
    const needing = Schema.String as unknown as Schema.Codec<string, string, FileSystem>
    // @ts-expect-error a field decoding under a service is no `Fields`, so `make` cannot read the file synchronously
    const refused = () => ConfigFile.make(PACK, { fields: { needing }, campaignFields: {}, check: () => undefined })
    const taken = ConfigFile.make(PACK, { fields: { needing: Schema.String }, campaignFields: {}, check: () => undefined })
    expect(refused).toBeTypeOf("function")
    expect(Result.isFailure(taken.parse("tool.toml", "needing = 3\n"))).toBe(true)
  })
})

const CHECKOUT = path.join(dir, "checkout.toml")
fs.writeFileSync(CHECKOUT, "[collection.full]\nalways = [\"x\"]\n")

const SOURCE: DeploymentConfig.Source<ConfigFile.Config<typeof FIELDS, typeof CAMPAIGN_FIELDS>> = {
  of: (env) => Layout.of(PACK, env),
  configOf: WorkspaceEnv.make(PACK, decodeText).configOf,
  decode: decodeText,
  checkoutFile: CHECKOUT,
  levels: { quick: { never: ["slow"] }, full: { never: ["y"] } }
}

describe("a deployment's configuration", () => {
  it("is the checkout's file where no workspace is named, the tooling's levels beside its own, its own winning", async () => {
    const stated = await run(DeploymentConfig.of(SOURCE, {}))
    expect(stated.file).toBe(CHECKOUT)
    expect(stated.config.collection).toEqual({ quick: { never: ["slow"] }, full: { always: ["x"] } })
    expect(DeploymentConfig.levelsOf(stated.config)).toEqual({ default: "standard", levels: ["standard", "quick", "full"] })
  })

  it("is the workspace's file where one is named", async () => {
    const ws = path.join(dir, "ws")
    fs.mkdirSync(ws)
    fs.writeFileSync(path.join(ws, "tool.toml"), "[tags.slow]\nlabel = \"Slow\"\ndefault = \"exclude\"\n")
    const stated = await run(DeploymentConfig.of(SOURCE, { TOOL_WORKSPACE: ws }))
    expect(stated.file).toBe(path.join(ws, "tool.toml"))
    expect(stated.config.tags).toEqual({ slow: { label: "Slow", default: "exclude" } })
  })

  it("is refused naming the checkout's file where it does not read", async () => {
    const refused = await run(Effect.flip(DeploymentConfig.of({ ...SOURCE, checkoutFile: path.join(dir, "none.toml") }, {})))
    expect(refused.reason).toMatch(new RegExp(`^${path.join(dir, "none.toml")}: `))
  })
})

const DEFINED = Campaigns.make<NonNullable<ConfigFile.Config<typeof FIELDS, typeof CAMPAIGN_FIELDS>["campaign"]>[string], {
  readonly coverage: boolean
}>({
  defaultName: "every-case",
  extra: (campaign) => ({ coverage: campaign?.coverage ?? true }),
  deployment: (env) => DeploymentConfig.of(SOURCE, env),
  inWorkspace: (env) => Layout.of(PACK, env) !== undefined
})

describe("a deployment's campaigns", () => {
  it("are the default one, the tooling's fields at their defaults, where the file defines none", () => {
    expect(DEFINED.definedFrom({})).toEqual([
      { name: "every-case", title: null, cases: [Campaigns.EVERY_CASE], lanes: null, allow: [], setup: [], coverage: true }
    ])
  })

  it("carry the tooling's fields as their file states them, and a checkout keeps the default one first", () => {
    const config = { campaign: { routing: { cases: ["x/**"] as const, coverage: false } } }
    expect(DEFINED.definedFrom(config)).toEqual([
      { name: "routing", title: "routing", cases: ["x/**"], lanes: null, allow: [], setup: [], coverage: false }
    ])
    expect(DEFINED.definedInCheckout(config).map((one) => one.name)).toEqual(["every-case", "routing"])
  })

  it("choose the one named, else the only one, else the default a checkout holds, or say why not", () => {
    const two = DEFINED.definedFrom({ campaign: { a: { cases: ["a"] as const }, b: { cases: ["b"] as const } } })
    expect(DEFINED.chosen(two, undefined)).toBe("several campaigns are defined, name one with --campaign: a, b")
    expect(DEFINED.chosen(two, "c")).toBe("no campaign c; the campaigns: a, b")
    const checkout = DEFINED.definedInCheckout({ campaign: { a: { cases: ["a"] as const } } })
    expect(DEFINED.chosen(checkout, undefined)).toMatchObject({ name: "every-case" })
  })
})
