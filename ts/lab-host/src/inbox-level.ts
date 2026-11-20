/**
 * The level an inbox capture is taken at (`@sip/workspace` `AnonymiseLevel`):
 * the one its SIDECAR states, else the workspace's `[anonymise] level`, else
 * the default. The sidecar is `<first file>.anon.json`, `{"level": "none"}`,
 * beside the capture's first file in the inbox: a drop on the site writes it
 * from the form, and whoever copies a file in may write it by hand. It is no
 * capture form, so no inbox read takes it for one; the take removes it with
 * the files it took.
 */
import { tomlOf } from "@sip/rig/manifest-file"
import { AnonymiseLevel } from "@sip/workspace"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { stamp } from "./inbox-scan.js"

/** The level stated for a capture could not be read; nothing is taken. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("InboxLevel.Unreadable", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

const SIDECAR_SUFFIX = ".anon.json"

/** The sidecar of the capture whose first file is `first`. */
export const sidecarOf = (first: string): string => `${first}${SIDECAR_SUFFIX}`

const Sidecar = Schema.Struct({ level: AnonymiseLevel.Level })
const decodeSidecar = Schema.decodeUnknownResult(Sidecar, { onExcessProperty: "error" })

const ConfigSection = Schema.Struct({ anonymise: Schema.optionalKey(AnonymiseLevel.Section) })
const decodeConfig = Schema.decodeUnknownResult(ConfigSection)

/** A level and where it was stated. */
export interface Stated {
  readonly level: AnonymiseLevel.Level
  /** `sidecar`, `config` (the workspace's `[anonymise] level`) or `default`. */
  readonly from: "sidecar" | "config" | "default"
}

/**
 * The sidecars among the inbox file names `names` whose capture is not among
 * them: a sidecar left by a capture gone states nothing for a later capture
 * of that name.
 */
export const orphans = (names: ReadonlyArray<string>): ReadonlyArray<string> => {
  const present = new Set(names)
  return names.filter((name) => name.endsWith(SIDECAR_SUFFIX) && !present.has(name.slice(0, -SIDECAR_SUFFIX.length)))
}

/** The sidecar text stating `level`. */
export const sidecarText = (level: AnonymiseLevel.Level): string => `${JSON.stringify({ level })}\n`

/**
 * The level the workspace configuration file `config` states, else the
 * default; a missing file states none. A file that does not parse, or whose
 * `[anonymise] level` names no level, is refused naming it: a capture is
 * never taken at a level nobody stated.
 */
export const workspaceDefault = Effect.fn("InboxLevel.workspaceDefault")(function*(config: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(config).pipe(Effect.option)
  if (text._tag === "None") return { level: AnonymiseLevel.DEFAULT, from: "default" } satisfies Stated
  const raw = tomlOf(text.value)
  if (Result.isFailure(raw)) {
    return yield* new Unreadable({ reason: `${path.basename(config)} does not parse, so its [anonymise] level does not read` })
  }
  const decoded = decodeConfig(raw.success)
  if (Result.isFailure(decoded)) {
    return yield* new Unreadable({
      reason: `${path.basename(config)} [anonymise] level names no level: write level = "<${AnonymiseLevel.LEVELS.join(" | ")}>"`
    })
  }
  const level = decoded.success.anonymise?.level
  if (level === undefined) return { level: AnonymiseLevel.DEFAULT, from: "default" } satisfies Stated
  return { level, from: "config" } satisfies Stated
})

/**
 * The sidecar of the capture whose first inbox file is `first` as it stands
 * (`inbox-scan.ts` `stamp`), null where there is none: a take refused at one
 * sidecar stamp is tried again once the sidecar changes.
 */
export const sidecarStamp = Effect.fn("InboxLevel.sidecarStamp")(function*(first: string) {
  const fs = yield* FileSystem.FileSystem
  const info = yield* fs.stat(sidecarOf(first)).pipe(Effect.option)
  if (info._tag === "None") return null
  const mtime = info.value.mtime
  return stamp({ name: path.basename(first), size: Number(info.value.size), mtimeMs: mtime._tag === "Some" ? mtime.value.getTime() : 0 })
})

/**
 * The level the capture whose first inbox file is `first` is taken at: its
 * sidecar's, else {@link workspaceDefault} of `config`. A sidecar that is no
 * `{"level": …}` naming a level is refused, naming the file and the levels.
 */
export const levelOf = Effect.fn("InboxLevel.levelOf")(function*(first: string, config: string) {
  const fs = yield* FileSystem.FileSystem
  const sidecar = sidecarOf(first)
  const text = yield* fs.readFileString(sidecar).pipe(Effect.option)
  if (text._tag === "None") return yield* workspaceDefault(config)
  const parsed = Result.try(() => JSON.parse(text.value) as unknown)
  const decoded = Result.isSuccess(parsed) ? decodeSidecar(parsed.success) : undefined
  if (decoded === undefined || Result.isFailure(decoded)) {
    return yield* new Unreadable({
      reason: `${path.basename(sidecar)} states no level: write {"level": "<${AnonymiseLevel.LEVELS.join(" | ")}>"}`
    })
  }
  return { level: decoded.success.level, from: "sidecar" } satisfies Stated
})
