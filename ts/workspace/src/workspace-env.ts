/**
 * The workspace applied to an environment (`@sip/cli` `HostEnv.before`):
 * where the workspace variable names one, its configuration file is read and
 * validated and the workspace's roots and keys (`layout.ts` `defaultsOf`) are
 * set where the environment states none. Applied before the checkout's env
 * files, so a workspace wins over them and loses only to an exported variable.
 *
 * A workspace without its configuration file, or with one the schema
 * refuses, stops the command, naming the file and the key.
 */
import type * as HostEnv from "@sip/cli/host-env"
import { Refused } from "@sip/cli/host-env"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type * as ConfigFile from "./config-file.js"
import * as Layout from "./layout.js"
import type { WorkspacePack } from "./workspace-pack.js"

/** How a tooling reads its configuration file's text (`config-file.ts` `make`). */
export type Decode<A extends ConfigFile.Base> = (file: string, text: string) => Effect.Effect<A, ConfigFile.Invalid>

/** The configuration of the workspace at `layout`, or why it has none. */
export const configOf = <A extends ConfigFile.Base>(pack: WorkspacePack, decode: Decode<A>, layout: Layout.Base) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(layout.config).pipe(
      Effect.mapError(() =>
        new Refused({
          reason: `${pack.variables.workspace}=${layout.root} holds no readable ${pack.configFile} ` +
            `(${layout.config}): \`${pack.initCommand} ${layout.root}\` writes one`
        })
      )
    )
    return yield* decode(layout.config, text).pipe(
      Effect.mapError((invalid) => new Refused({ reason: invalid.message }))
    )
  })

/** The workspace `env` names applied to `env`; nothing where it names none. */
export const apply = <A extends ConfigFile.Base>(
  pack: WorkspacePack,
  decode: Decode<A>,
  env: NodeJS.ProcessEnv
): Effect.Effect<ReadonlyArray<HostEnv.Applied>, Refused, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const layout = Layout.of(pack, env)
    if (layout === undefined) return []
    const config = yield* configOf(pack, decode, layout)
    env[pack.variables.workspace] = layout.root
    const set: Array<string> = []
    const ignored: Array<string> = []
    for (const [key, value] of Object.entries(Layout.defaultsOf(pack, layout, config))) {
      if (env[key] !== undefined) {
        ignored.push(key)
        continue
      }
      env[key] = value
      set.push(key)
    }
    return [{ file: layout.config, set, ignored }]
  })

/** Every function above bound to `pack` and `decode`, its arguments otherwise the same. */
export const make = <A extends ConfigFile.Base>(pack: WorkspacePack, decode: Decode<A>) => ({
  configOf: (layout: Layout.Base) => configOf(pack, decode, layout),
  apply: (env: NodeJS.ProcessEnv) => apply(pack, decode, env)
})
