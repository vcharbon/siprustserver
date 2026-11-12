/**
 * A workspace file's default export (or all its exports), imported as the
 * workspace's directories stand on disk: the SDK resolve hook registered
 * first, and the directories and their version (`versionOf`: every `.ts`
 * file's path, size and time) in the URL's query. The hook carries that
 * query onto every module of those directories the file imports, so a
 * long-lived process reads an edited file, or an edited module a file
 * imports, anew, an unchanged workspace as the modules it already imported,
 * and a module two loaded files both import as one module.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import { createHash } from "node:crypto"
import * as path from "node:path"
import { pathToFileURL } from "node:url"
import * as ResolveHook from "./resolve-hook.js"
import type { SdkModule } from "./sdk-module.js"

export class FileUnloadable extends Schema.TaggedError<FileUnloadable>()("CaseModules.FileUnloadable", {
  file: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.file}: does not load: ${this.reason}`
  }
}

/** The directories a load reads, and their version. */
export interface Stamp {
  readonly roots: ReadonlyArray<string>
  readonly version: string
}

/** An error's message on one line: a transform error names its file and cause on the lines after its first. */
const reasonOf = (e: unknown): string =>
  (e instanceof Error ? e.message : String(e)).split("\n").map((line) => line.trim()).filter((line) => line !== "").join(" ")

/** The version of the directories `roots` (those that exist): a hash of each `.ts` file's path, size and time. */
export const versionOf = (...roots: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const hash = createHash("sha256")
    for (const root of roots) {
      if (!(yield* fs.exists(root))) continue
      const names = (yield* fs.readDirectory(root, { recursive: true }))
        .filter((name) => name.endsWith(".ts") && !name.split(path.sep).includes("node_modules"))
        .sort()
      for (const name of names) {
        const info = yield* fs.stat(path.join(root, name))
        hash.update(`${path.join(root, name)}\0${info.size}\0${info.mtime._tag === "Some" ? info.mtime.value.getTime() : 0}\n`)
      }
    }
    return { roots, version: hash.digest("hex").slice(0, 16) } satisfies Stamp
  })

/** Every export of the workspace file at `absolute` in the directories `stamp` names, `sdk` resolved, `file` in a refusal. */
export const moduleExports = (sdk: SdkModule, absolute: string, file: string, stamp: Stamp) =>
  Effect.andThen(
    ResolveHook.ensure(sdk),
    Effect.tryPromise({
      try: async () => {
        const url = `${pathToFileURL(absolute).href}${ResolveHook.queryOf(stamp.roots, stamp.version)}`
        return (await import(url)) as Readonly<Record<string, unknown>>
      },
      catch: (e) => new FileUnloadable({ file, reason: reasonOf(e) })
    })
  )

/** The default export of the workspace file at `absolute` in the directories `stamp` names, `sdk` resolved, `file` in a refusal. */
export const defaultExport = (sdk: SdkModule, absolute: string, file: string, stamp: Stamp) =>
  Effect.map(moduleExports(sdk, absolute, file, stamp), (exported) => exported.default)
