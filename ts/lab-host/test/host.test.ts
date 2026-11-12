/**
 * The host's session, boxes and builds read every name they state off the
 * tooling's pack: the none-variable, the interpreter's wrapper and its
 * variables, what the build wrapper is told, and the prebuilt directory's
 * variable in a refusal.
 */
import { NodeServices } from "@effect/platform-node"
import { Boxing as CorpusBoxing, Roots } from "@sip/corpus"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as Boxing from "../src/boxing.js"
import * as Builds from "../src/builds.js"
import * as InterpreterEnv from "../src/interpreter-env.js"
import * as UserSession from "../src/user-session.js"
import { PACK } from "./pack.js"

describe("user session", () => {
  it("an install stating none under its pack's variable asks nothing and says nothing", async () => {
    const said: Array<string> = []
    const answer = await Effect.runPromise(
      UserSession.askedOf(PACK.variables.userSession, { TOOL_USER_SESSION: "none" }, (line) =>
        Effect.sync(() => said.push(line)))
    )
    expect([answer, said]).toEqual([false, []])
  })

  it("another tooling's none-variable is not this pack's: the missing session is said", async () => {
    const said: Array<string> = []
    const answer = await Effect.runPromise(
      UserSession.askedOf(PACK.variables.userSession, { OTHER_USER_SESSION: "none" }, (line) =>
        Effect.sync(() => said.push(line)))
    )
    expect(answer).toBe(false)
    expect(said).toHaveLength(1)
    expect(said[0]).toContain("XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS unset")
  })
})

describe("interpreter environment", () => {
  it("names the wrapper's memory and the ledger by the pack's variables, the box only where a session answers", () => {
    expect(InterpreterEnv.envBlock(PACK, { memoryMax: "2G", ledgerDir: "/l" }, true)).toEqual({
      TOOL_MEMORY_MAX: "2G",
      TOOL_LEDGER: "/l"
    })
    expect(InterpreterEnv.envBlock(PACK, { memoryMax: "2G" }, false)).toEqual({})
  })

  it("the boxing hands a cell the pack's wrapper and its sized box", async () => {
    const seen = await Effect.runPromise(
      Effect.gen(function*() {
        const boxing = yield* CorpusBoxing.Service
        return { wrapper: yield* boxing.interpreter, vars: boxing.interpreterVariables("1G") }
      }).pipe(Effect.provide(Boxing.layerFor(PACK, true)))
    )
    expect(seen).toEqual({ wrapper: "/tool/bin/boxed", vars: { TOOL_MEMORY_MAX: "1G" } })
  })
})

describe("builds", () => {
  const dirs: Array<string> = []
  afterEach(() => {
    for (const dir of dirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
  })

  it("a build in a capped scope tells the pack's wrapper of it; one outside keeps the environment", () => {
    expect(Builds.buildEnv(PACK, { A: "1" }, true)).toEqual({ A: "1", TOOL_POOL: "off", TOOL_SCOPE: "2G" })
    expect(Builds.buildEnv(PACK, { A: "1" }, false)).toEqual({ A: "1" })
  })

  it("a build's scope is capped as the pack states: its memory ceiling, then its other properties", () => {
    expect(Builds.scopeOf(PACK)).toEqual(["MemoryMax=2G", "CPUQuota=200%"])
  })

  it("a prebuilt directory lacking the binary is refused by the pack's variable, nothing built", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lab-host-bin-"))
    dirs.push(dir)
    const before = process.env.TOOL_BIN_DIR
    process.env.TOOL_BIN_DIR = dir
    try {
      const error = await Effect.runPromise(
        Effect.flip(Builds.ready(PACK, Builds.sipflow(PACK))).pipe(
          Effect.provide(Layer.mergeAll(NodeServices.layer, Roots.layer))
        )
      )
      expect(error.message).toBe(`sipflow: no executable sipflow in TOOL_BIN_DIR ${dir}`)
    } finally {
      if (before === undefined) delete process.env.TOOL_BIN_DIR
      else process.env.TOOL_BIN_DIR = before
    }
  })

  it("an executable in the prebuilt directory is taken as it is", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lab-host-bin-"))
    dirs.push(dir)
    fs.writeFileSync(path.join(dir, "pivot-schema"), "#!/bin/sh\n", { mode: 0o755 })
    const before = process.env.TOOL_BIN_DIR
    process.env.TOOL_BIN_DIR = dir
    try {
      const file = await Effect.runPromise(
        Builds.ready(PACK, Builds.pivotSchema(PACK)).pipe(
          Effect.provide(Layer.mergeAll(NodeServices.layer, Roots.layer))
        )
      )
      expect(file).toBe(path.join(dir, "pivot-schema"))
    } finally {
      if (before === undefined) delete process.env.TOOL_BIN_DIR
      else process.env.TOOL_BIN_DIR = before
    }
  })
})
