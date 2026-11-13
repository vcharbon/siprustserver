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

/** The level stated for a capture could not be read; nothing is taken. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("InboxLevel.Unreadable", { reason: Schema.String }) {
  override get message(): string {
    return this.reason
  }
}

/** The sidecar of the capture whose first file is `first`. */
export const sidecarOf = (first: string): string => `${first}.anon.json`

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

/** The sidecar text stating `level`. */
export const sidecarText = (level: AnonymiseLevel.Level): string => `${JSON.stringify({ level })}\n`

/** The level the workspace configuration file `config` states, else the default; a file that does not read states none. */
export const workspaceDefault = Effect.fn("InboxLevel.workspaceDefault")(function*(config: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(config).pipe(Effect.option)
  if (text._tag === "None") return { level: AnonymiseLevel.DEFAULT, from: "default" } satisfies Stated
  const raw = tomlOf(text.value)
  const decoded = Result.isSuccess(raw) ? decodeConfig(raw.success) : undefined
  const level = decoded !== undefined && Result.isSuccess(decoded) ? decoded.success.anonymise?.level : undefined
  if (level === undefined) return { level: AnonymiseLevel.DEFAULT, from: "default" } satisfies Stated
  return { level, from: "config" } satisfies Stated
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
