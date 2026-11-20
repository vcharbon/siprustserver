/**
 * A deployment's RIG PATH (upstream ADR-0060) and the check a job entry point
 * makes before the job opens: the pack's directory (`pack.toml`), the
 * workspace the environment names, else the pack's shipped defaults; the
 * deployment's `[collection]` levels and every lane file's `[collectors]`,
 * each lane loaded with its parts, are checked against the collectors the
 * path defines (`@sip/rig` `Collection.check`) and the pack's own rules, and
 * each named campaign's `setup` names operations the path defines. A refusal
 * names the file or files at fault. A job's layer runs it ({@link checked}),
 * so a line the parser answers itself (`--help`, a usage error) is no job and
 * checks nothing.
 */
import { Collection, RigPath } from "@sip/rig"
import type { ConfigFile, DeploymentConfig } from "@sip/workspace"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { CliError } from "effect/unstable/cli"

/** What a pack's rig path and its job check are read from. */
export interface WiringPack<A extends ConfigFile.Base> {
  /** The pack: the directory holding `pack.toml`. */
  readonly pack: string
  /** The workspace defaults the checkout ships, where no workspace is named. */
  readonly defaults: string
  /** The root of the workspace `env` names, if any. */
  readonly workspaceRoot: (env: NodeJS.ProcessEnv) => string | undefined
  /** The configuration the deployment `env` names, and its file. */
  readonly deployment: (
    env: NodeJS.ProcessEnv
  ) => Effect.Effect<DeploymentConfig.Stated<A>, { readonly message: string }, FileSystem.FileSystem>
  /** Every lane file's wiring of the deployment `env` names, each lane loaded with its parts. */
  readonly lanes: (
    rig: RigPath.RigPath,
    env: NodeJS.ProcessEnv
  ) => Effect.Effect<ReadonlyArray<Collection.LaneWiring>, { readonly message: string }, FileSystem.FileSystem>
  /** Why the pack's collectors cannot run under these levels and lanes; `undefined` where they can. */
  readonly refusal: (
    rig: RigPath.RigPath,
    levels: { readonly file: string; readonly collection: Collection.Collection | undefined },
    lanes: ReadonlyArray<Collection.LaneWiring>
  ) => string | undefined
}

/** The rig path's roots for the deployment `env` names. */
export const rootsOf = <A extends ConfigFile.Base>(pack: WiringPack<A>, env: NodeJS.ProcessEnv): RigPath.Roots => ({
  pack: pack.pack,
  workspace: pack.workspaceRoot(env) ?? pack.defaults
})

/**
 * Why a named campaign of `config`, the file `file` holds, may not run: its
 * setup names an operation `rig` does not define, or one operation twice; or
 * `null`.
 */
export const setupRefusalOf = (rig: RigPath.RigPath, file: string, config: ConfigFile.Base): string | null => {
  for (const [name, campaign] of Object.entries(config.campaign ?? {})) {
    const setup = campaign.setup ?? []
    const unknown = setup.find((operation) => !rig.operations.has(operation))
    if (unknown !== undefined) return `${file}: campaign.${name}.setup: names ${unknown}, which no operation of the path defines`
    const twice = setup.find((operation, at) => setup.indexOf(operation) !== at)
    if (twice !== undefined) return `${file}: campaign.${name}.setup: names ${twice} twice`
  }
  return null
}

/**
 * Why a job of the deployment `env` names may not open, or `null`: the rig
 * path refused, or the collection level `level` (absent, the deployment's
 * default), the levels, a lane file or a named campaign's setup name what the
 * path does not hold, or what the pack's collectors cannot run under.
 */
export const refusal = <A extends ConfigFile.Base>(
  pack: WiringPack<A>,
  env: NodeJS.ProcessEnv,
  level?: string
): Effect.Effect<string | null, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const rig = yield* RigPath.load(rootsOf(pack, env))
    const deployment = yield* pack.deployment(env)
    const lanes = yield* pack.lanes(rig, env)
    const levels = { file: deployment.file, collection: deployment.config.collection }
    const checked = Collection.check(rig, { workspace: levels, level, lanes })
    if (checked._tag === "Failure") return checked.failure.message
    return pack.refusal(rig, levels, lanes) ?? setupRefusalOf(rig, deployment.file, deployment.config)
  }).pipe(Effect.catch((error: { readonly message: string }) => Effect.succeed(error.message)))

/**
 * The layer a job's handler is built over: {@link refusal} of the deployment
 * `env` names and the collection level `level` the job names, once the env
 * files are applied, a refusal failing the handler as a user error, which the
 * runner prints and exits 2 on.
 */
export const checked = <A extends ConfigFile.Base>(
  pack: WiringPack<A>,
  env: NodeJS.ProcessEnv,
  level?: string
): Layer.Layer<never, CliError.UserError, FileSystem.FileSystem> =>
  Layer.effectDiscard(
    Effect.flatMap(refusal(pack, env, level), (refused) =>
      refused === null ? Effect.void : Effect.fail(new CliError.UserError({ cause: refused, userMessage: refused })))
  )
