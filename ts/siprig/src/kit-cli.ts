/**
 * `kit-cli check <pack-dir> [--staged <dir>]`, the image kit's reading of a
 * pack for its front door (`tooling/siprig/kit`): the pack's facts
 * (`pack-image.ts`), its tree read in the staged copy `--staged` names, one
 * `KEY=value` line each (`NAME`, `IMAGE`, `WRAPPER`, `COMPONENT`, `PACKAGES`
 * space-separated, `BINARIES` and `SETUP` empty where unstated), or the
 * refusal on stderr and exit status 2.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import * as PackImage from "./pack-image.ts"

/** The lines `facts` print as. */
export const linesOf = (facts: PackImage.Facts): ReadonlyArray<string> => [
  `NAME=${facts.name}`,
  `IMAGE=${facts.image}`,
  `WRAPPER=${facts.wrapper}`,
  `COMPONENT=${facts.component}`,
  `PACKAGES=${facts.packages.join(" ")}`,
  `BINARIES=${facts.binaries ?? ""}`,
  `SETUP=${facts.setup ?? ""}`
]

const check = Command.make("check", {
  dir: Argument.string("pack-dir").pipe(Argument.withDescription("The pack's directory, holding pack.toml")),
  staged: Flag.string("staged").pipe(
    Flag.withDescription("The pack's tree as the image receives it; default the pack's directory"),
    Flag.optional
  )
}, (input) =>
  PackImage.read(input.dir, Option.getOrUndefined(input.staged)).pipe(
    Effect.flatMap((facts) => Effect.forEach(linesOf(facts), (line) => Console.log(line), { discard: true })),
    Effect.catch((refusal) => Effect.andThen(Console.error(`kit: ${refusal.message}`), Effect.sync(() => (process.exitCode = 2))))
  )).pipe(Command.withDescription("Read a pack's manifest as the kit builds from it, or refuse it naming the key"))

const command = Command.make("kit-cli").pipe(Command.withSubcommands([check]))

Command.run(command, { version: "0.0.0" }).pipe(Effect.provide(NodeServices.layer), NodeRuntime.runMain)
