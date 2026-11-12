/**
 * Every binary follows the rule: from the prebuilt directory the rule's
 * variable names, else the checkout's release build; the toolchain's binary
 * variables fall back to it, a variable the environment sets still winning,
 * and the variable is read when the toolchain layer is built.
 */
import * as Config from "effect/Config"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { type BinaryRule, binaryPath, binDir, toolchainBinaries } from "../src/binary-rule.js"

const RULE: BinaryRule = { variable: "HOST_TOOLS_TEST_BIN_DIR", root: "/checkout" }

afterEach(() => {
  delete process.env[RULE.variable]
})

/** The toolchain layer over an environment that states only `env`. */
const over = (layer: Layer.Layer<never>, env: Record<string, string>) =>
  layer.pipe(Layer.provide(ConfigProvider.layer(ConfigProvider.fromEnv({ env }))))

const toolchain = Effect.all([Config.string("REPLAY_BIN"), Config.string("SIPFLOW"), Config.string("PIVOT_SCHEMA_BIN")])

describe("a binary's path", () => {
  it("is the checkout's release build where the variable is unset or empty", () => {
    expect(binDir(RULE, {})).toBeUndefined()
    expect(binDir(RULE, { [RULE.variable]: "" })).toBeUndefined()
    expect(binaryPath(RULE, "replay", {})).toBe("/checkout/target/release/replay")
    expect(binaryPath(RULE, "replay", { [RULE.variable]: "" })).toBe("/checkout/target/release/replay")
  })

  it("is in the prebuilt directory the variable names, made absolute", () => {
    expect(binaryPath(RULE, "replay", { [RULE.variable]: "/opt/bin" })).toBe("/opt/bin/replay")
    expect(binDir(RULE, { [RULE.variable]: "rel/bin" })).toBe(path.resolve("rel/bin"))
    expect(binaryPath(RULE, "sipflow", { OTHER_BIN_DIR: "/opt/bin" })).toBe("/checkout/target/release/sipflow")
  })
})

describe("the toolchain's binary variables", () => {
  it("point where the rule says, a variable the environment sets still winning", async () => {
    const layer = toolchainBinaries(RULE, { [RULE.variable]: "/opt/bin" })
    expect(await Effect.runPromise(Effect.provide(toolchain, over(layer, {})))).toEqual([
      "/opt/bin/replay",
      "/opt/bin/sipflow",
      "/opt/bin/pivot-schema"
    ])
    const stated = over(layer, { SIPFLOW: "/elsewhere/sipflow" })
    expect(await Effect.runPromise(Effect.provide(Config.string("SIPFLOW"), stated))).toBe("/elsewhere/sipflow")
  })

  it("read the process environment when the layer is built, not when it is declared", async () => {
    const declared = toolchainBinaries(RULE)
    process.env[RULE.variable] = "/later/bin"
    expect(await Effect.runPromise(Effect.provide(Config.string("REPLAY_BIN"), over(declared, {})))).toBe(
      "/later/bin/replay"
    )
  })
})
