/**
 * The configuration of a deployment, in the one form both kinds state it: a
 * workspace's configuration file, else the checkout's own file. Read anew at
 * each call, so the CLI and the site read one file the same way; the levels
 * the tooling offers every deployment stand beside the file's own.
 */
import { Refused } from "@sip/cli/host-env"
import { Collection } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import type * as ConfigFile from "./config-file.js"
import type * as Layout from "./layout.js"
import type * as WorkspaceEnv from "./workspace-env.js"

/** The collection levels a job may name, and the one a job naming none takes. */
export interface Levels {
  readonly default: string
  readonly levels: ReadonlyArray<string>
}

/**
 * The collection levels a job of a deployment stating `config` may name:
 * `standard` and every level its `[collection]` defines, in its order. A
 * configuration that does not read is offered `standard` alone: the job
 * itself refuses it before it opens, naming the file.
 */
export const levelsOf = (config: Pick<ConfigFile.Base, "collection">): Levels => {
  const read = Collection.levelsOf(config.collection)
  if (Result.isFailure(read)) return { default: Collection.STANDARD, levels: [Collection.STANDARD] }
  return { default: read.success.default, levels: [Collection.STANDARD, ...read.success.levels.keys()] }
}

/** A deployment's configuration and the file stating it. */
export interface Stated<A> {
  readonly file: string
  readonly config: A
}

/** Where a tooling's deployments are read from. */
export interface Source<A extends ConfigFile.Base> {
  /** The workspace `env` names, if any. */
  readonly of: (env: NodeJS.ProcessEnv) => Layout.Base | undefined
  /** A workspace's configuration (`workspace-env.ts` `configOf`). */
  readonly configOf: (layout: Layout.Base) => Effect.Effect<A, Refused, FileSystem.FileSystem>
  /** How the file's text reads. */
  readonly decode: WorkspaceEnv.Decode<A>
  /** The checkout's file, read where no workspace is named. */
  readonly checkoutFile: string
  /** The levels the tooling offers every deployment; a file defining a level of the same name replaces it. */
  readonly levels: Collection.Collection
}

/** The configuration the deployment `env` names, as its file states it. */
export const stated = <A extends ConfigFile.Base>(
  source: Source<A>,
  env: NodeJS.ProcessEnv
): Effect.Effect<Stated<A>, Refused, FileSystem.FileSystem> => {
  const layout = source.of(env)
  if (layout !== undefined) return Effect.map(source.configOf(layout), (config) => ({ file: layout.config, config }))
  return Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(source.checkoutFile).pipe(
      Effect.mapError((e) => new Refused({ reason: `${source.checkoutFile}: ${e.message}` }))
    )
    const config = yield* source.decode(source.checkoutFile, text).pipe(
      Effect.mapError((e) => new Refused({ reason: e.message }))
    )
    return { file: source.checkoutFile, config }
  })
}

/** The configuration the deployment `env` names, the tooling's levels beside its own, and the file stating it; or why it does not read. */
export const of = <A extends ConfigFile.Base>(
  source: Source<A>,
  env: NodeJS.ProcessEnv
): Effect.Effect<Stated<A>, Refused, FileSystem.FileSystem> =>
  Effect.map(stated(source, env), ({ file, config }) => ({
    file,
    config: { ...config, collection: { ...source.levels, ...config.collection } }
  }))
