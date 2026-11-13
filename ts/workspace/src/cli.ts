/**
 * The generic verbs and lines of a tooling's workspace command: `check
 * [<dir>]` validates the configuration file and prints the environment it
 * states, a URL's password masked; a verb naming no directory takes the one
 * the workspace variable names; what `init` made is printed one line per path.
 */
import type { Refused } from "@sip/cli/host-env"
import * as Runner from "@sip/cli/runner"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import { Argument, Command } from "effect/unstable/cli"
import type * as ConfigFile from "./config-file.js"
import type * as Init from "./init.js"
import * as Layout from "./layout.js"
import type { WorkspacePack } from "./workspace-pack.js"

/** A value as `check` prints it: a URL's password masked. */
export const shown = (value: string): string => {
  const url = URL.parse(value)
  if (url === null || url.password === "") return value
  url.password = "***"
  return url.toString()
}

/** The optional directory argument of a verb over one workspace. */
export const dirArgument = (pack: WorkspacePack) =>
  Argument.string("dir").pipe(
    Argument.withDescription(`The workspace directory; default ${pack.variables.workspace}`),
    Argument.optional
  )

/**
 * The workspace a verb works on: the directory named, else the one the
 * workspace variable names; `undefined`, the exit code 2 and a line naming
 * both where neither is.
 */
export const layoutNamed = <D extends string>(
  pack: WorkspacePack<D>,
  program: string,
  dir: Option.Option<string>
): Effect.Effect<Layout.Layout<D> | undefined> =>
  Effect.gen(function*() {
    const named = Option.getOrUndefined(dir)
    const layout = named === undefined ? Layout.of(pack, process.env) : Layout.layoutOf(pack, named)
    if (layout !== undefined) return layout
    yield* Console.error(`${program}: name a directory or set ${pack.variables.workspace}`)
    yield* Runner.exit(2)
    return undefined
  })

/**
 * What `check` verifies of a workspace beyond its configuration file (the
 * files a tooling reads from it at start), or the refusal naming the file at
 * fault.
 */
export type Verify<A extends ConfigFile.Base> = (
  layout: Layout.Base,
  config: A
) => Effect.Effect<void, { readonly message: string }, FileSystem.FileSystem>

/**
 * The `check` verb of `program`: the file validated, then the rest of the
 * workspace `verify` reads, and the environment the file states printed;
 * exit 2 with the refusal where either does not read.
 */
export const check = <A extends ConfigFile.Base>(
  pack: WorkspacePack,
  configOf: (layout: Layout.Base) => Effect.Effect<A, Refused, FileSystem.FileSystem>,
  program: string,
  verify: Verify<A> = () => Effect.void
) =>
  Command.make("check", { dir: dirArgument(pack) }, (input) =>
    Effect.gen(function*() {
      const layout = yield* layoutNamed(pack, program, input.dir)
      if (layout === undefined) return
      const config = yield* configOf(layout).pipe(
        Effect.catchTag("HostEnv.Refused", (refused) =>
          Effect.andThen(Console.error(refused.reason), Effect.as(Runner.exit(2), undefined)))
      )
      if (config === undefined) return
      const verified = yield* verify(layout, config).pipe(
        Effect.as(true),
        Effect.catch((refusal) => Effect.andThen(Console.error(refusal.message), Effect.as(Runner.exit(2), false)))
      )
      if (!verified) return
      yield* Console.log(`${layout.config}: valid`)
      for (const [key, value] of Object.entries(Layout.defaultsOf(pack, layout, config))) {
        yield* Console.log(`${key}=${shown(value)}`)
      }
    })).pipe(Command.withDescription(`Validate ${pack.configFile} and print the environment it states`))

/** What `init` made, one line per path, the workspace's root first. */
export const madeLines = <D extends string>(made: Init.Made<D>): ReadonlyArray<string> => [
  `workspace ${made.layout.root}`,
  ...made.directories.map((dir) => `  made   ${dir}`),
  ...(made.config === undefined ? [] : [`  wrote  ${made.config}`]),
  ...(made.census === undefined ? [] : [`  wrote  ${made.census} (an empty census run: nothing swept yet)`])
]
