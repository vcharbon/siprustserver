/**
 * The host's env files applied to this process (`CliFacts.hostEnv`): `KEY=VALUE`
 * lines, `#` comments and blanks, a quoted value keeping its inside. A file is
 * a DEFAULT, never an override — a key already exported is left as the caller
 * set it — and a key the facts do not list is ignored and said, because a typo
 * in an env file is otherwise silence. A path key is taken relative to the
 * directory of the file that states it; an absolute one is kept.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import type { HostEnv } from "./facts.js"

/** What one file contributed: itself, the keys it set, and the keys it could not. */
export interface Applied {
  readonly file: string
  readonly set: ReadonlyArray<string>
  /** Keys the file states that were already exported, or that the facts do not list. */
  readonly ignored: ReadonlyArray<string>
}

/** What the runner applied before the command ran; empty where it applied nothing. */
export const Applied: Context.Reference<ReadonlyArray<Applied>> = Context.Reference("@sip/cli/HostEnv/Applied", {
  defaultValue: (): ReadonlyArray<Applied> => []
})

/** The file's keys and values, the first statement of a key winning. */
export const parse = (text: string): ReadonlyMap<string, string> => {
  const read = new Map<string, string>()
  for (const line of text.split("\n")) {
    const trimmed = line.trim()
    if (trimmed === "" || trimmed.startsWith("#")) continue
    const at = trimmed.indexOf("=")
    if (at <= 0) continue
    const key = trimmed.slice(0, at).trim().replace(/^export\s+/, "")
    const value = trimmed.slice(at + 1).trim()
    const unquoted = /^(["']).*\1$/.test(value) ? value.slice(1, -1) : value
    if (!read.has(key)) read.set(key, unquoted)
  }
  return read
}

/** The host's env files applied to `env`, each answering what it did; a file the host lacks is skipped. */
export const apply = Effect.fn("HostEnv.apply")(function*(host: HostEnv, env: NodeJS.ProcessEnv = process.env) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const applied: Array<Applied> = []
  for (const file of host.files) {
    const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => undefined))
    if (text === undefined) continue
    const set: Array<string> = []
    const ignored: Array<string> = []
    for (const [key, value] of parse(text)) {
      if (!(key in host.keys) || env[key] !== undefined) {
        ignored.push(key)
        continue
      }
      env[key] = host.pathKeys.has(key) ? path.resolve(path.dirname(file), value) : value
      set.push(key)
    }
    applied.push({ file, set, ignored })
  }
  return applied as ReadonlyArray<Applied>
})
