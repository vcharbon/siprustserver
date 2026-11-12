/**
 * Where a tooling's Rust binaries are: one rule for every binary it execs.
 *
 * The rule's variable, where the environment sets it, names a directory of
 * prebuilt binaries: every binary is taken from it and nothing is built (an
 * install with no cargo and no source). Unset or empty, a binary is the
 * checkout's release build (`<root>/target/release/<name>`), which the tooling
 * brings up to date before use.
 */
import * as ConfigProvider from "effect/ConfigProvider"
import * as Layer from "effect/Layer"
import * as path from "node:path"

/** The rule as a tooling states it. */
export interface BinaryRule {
  /** The variable naming the prebuilt binary directory. */
  readonly variable: string
  /** The checkout whose release build a binary is where no prebuilt directory is named. */
  readonly root: string
}

/** The prebuilt binary directory `env` names under `rule`, absolute; `undefined` where it names none. */
export const binDir = (rule: BinaryRule, env: NodeJS.ProcessEnv = process.env): string | undefined => {
  const stated = env[rule.variable]
  return stated === undefined || stated === "" ? undefined : path.resolve(stated)
}

/** The checkout's release build of the binary `name`, where cargo puts it. */
export const releaseBuild = (rule: BinaryRule, name: string): string =>
  path.join(rule.root, "target", "release", name)

/** Where the binary `name` is under `env`: in the prebuilt directory, else the checkout's release build. */
export const binaryPath = (rule: BinaryRule, name: string, env: NodeJS.ProcessEnv = process.env): string => {
  const dir = binDir(rule, env)
  return dir === undefined ? releaseBuild(rule, name) : path.join(dir, name)
}

/**
 * This package's binary variables (`binaries.ts`: `REPLAY_BIN`, `SIPFLOW`,
 * `PIVOT_SCHEMA_BIN`) stated by {@link binaryPath}, as a fallback: a variable
 * the environment sets still wins. `env` (default `process.env`) is read when
 * the layer is built, so a prebuilt directory the host's env files name at
 * start is seen.
 */
export const toolchainBinaries = (rule: BinaryRule, env?: NodeJS.ProcessEnv): Layer.Layer<never> =>
  Layer.suspend(() =>
    ConfigProvider.layerAdd(
      ConfigProvider.fromUnknown({
        REPLAY_BIN: binaryPath(rule, "replay", env ?? process.env),
        SIPFLOW: binaryPath(rule, "sipflow", env ?? process.env),
        PIVOT_SCHEMA_BIN: binaryPath(rule, "pivot-schema", env ?? process.env)
      })
    )
  )
