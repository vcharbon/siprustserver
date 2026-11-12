/**
 * `siprig-workspace`, the base image's workspace command for a pack that
 * ships none (`base-workspace.ts`):
 *
 *   siprig-workspace init <dir>     every fixed subdirectory, a commented rig.toml, an empty census run
 *   siprig-workspace check [<dir>]  rig.toml validated, the environment it states printed
 *
 * `check` reads `SIPRIG_WORKSPACE` where no directory is named. A refused
 * `rig.toml` exits 2 naming the file and the key.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import { Cli, Init } from "@sip/workspace"
import * as Clock from "effect/Clock"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { Argument, Command } from "effect/unstable/cli"
import { configOf, PACK, TEMPLATE } from "./base-workspace.ts"

const PROGRAM = "siprig-workspace"

const init = Command.make("init", {
  dir: Argument.string("dir").pipe(Argument.withDescription("The workspace directory, made where missing"))
}, (input) =>
  Effect.gen(function*() {
    const made = yield* Init.prepare(PACK, { template: TEMPLATE, placed: {} }, input.dir, yield* Clock.currentTimeMillis)
    for (const line of Cli.madeLines(made)) yield* Console.log(line)
    yield* Console.log(`${PACK.variables.workspace}=${made.layout.root}`)
  })).pipe(Command.withDescription("Make a workspace ready; what it holds is kept"))

const command = Command.make(PROGRAM).pipe(Command.withSubcommands([init, Cli.check(PACK, configOf, PROGRAM)]))

Command.run(command, { version: "0.0.0" }).pipe(Effect.provide(NodeServices.layer), NodeRuntime.runMain)
