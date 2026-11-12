/**
 * The optional host tools a feature of the tooling execs (`cargo`,
 * `systemd-run`, a tracker's CLI, …): whether one is on `PATH`, and the words a feature says
 * where its tool is absent. A feature whose tool is missing reports itself
 * unavailable in those words rather than failing on a spawn error.
 */
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"

/** What a feature whose tool is missing says, after its own name. */
export const NOT_AVAILABLE = "not available in this install"

/** `feature`'s unavailability, naming the tool it lacks. */
export const unavailable = (feature: string, tool: string): string =>
  `${feature}: ${NOT_AVAILABLE} (no ${tool} on PATH)`

/** Whether `file` is an executable regular file. */
const executable = (file: string): boolean => {
  try {
    fs.accessSync(file, fs.constants.X_OK)
    return fs.statSync(file).isFile()
  } catch {
    return false
  }
}

/** Whether `file` is an executable regular file. */
export const isExecutable = (file: string): Effect.Effect<boolean> => Effect.sync(() => executable(file))

/** Whether an executable `tool` is in a directory of `env`'s `PATH`. */
export const onPath = (tool: string, env: NodeJS.ProcessEnv = process.env): Effect.Effect<boolean> =>
  Effect.sync(() =>
    (env["PATH"] ?? "").split(path.delimiter).some((dir) => dir !== "" && executable(path.join(dir, tool)))
  )
