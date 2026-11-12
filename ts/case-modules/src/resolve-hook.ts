/**
 * The resolve hook that gives a workspace's files their SDK: the specifier of
 * each SDK module {@link ensure} was given, imported from any module,
 * resolves to the tooling's own entry, so a workspace holds no module of the
 * tooling and what it loads is built by the code that reads it. A module
 * under one of the directories a file was loaded with (`load.ts`), imported
 * by that file, takes the same query, so it is read at the same version.
 * Registered once per process, before the first workspace file is imported.
 */
import * as Effect from "effect/Effect"
import { registerHooks } from "node:module"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import type { SdkModule } from "./sdk-module.js"

/** The query a module of the directories `roots` is imported with at `version`. */
export const queryOf = (roots: string | ReadonlyArray<string>, version: string): string =>
  `?${(typeof roots === "string" ? [roots] : roots).map((root) => `campaign=${encodeURIComponent(root)}`).join("&")}&version=${version}`

/** Whether `file` lies under `root`, outside any `node_modules`. */
const under = (root: string, file: string): boolean => {
  const relative = path.relative(root, file)
  return !(relative.startsWith("..") || path.isAbsolute(relative) || relative.split(path.sep).includes("node_modules"))
}

/** The query `parent` carries onto `resolved`, where `resolved` lies under one of the directories it names; else none. */
export const carriedOnto = (parent: string | undefined, resolved: string): string | undefined => {
  if (parent === undefined || !resolved.startsWith("file:") || resolved.includes("?")) return undefined
  const query = URL.parse(parent)?.searchParams
  const roots = query?.getAll("campaign") ?? []
  const version = query?.get("version")
  if (roots.length === 0 || version == null) return undefined
  const file = fileURLToPath(resolved)
  if (!roots.some((root) => under(root, file))) return undefined
  return `${resolved}${queryOf(roots, version)}`
}

/** The entry each SDK specifier resolves to, as the last {@link ensure} of it stated. */
const entries = new Map<string, string>()

let registered = false

/** The hook registered, once, resolving `sdk`'s specifier to its entry from then on. */
export const ensure = (sdk: SdkModule) =>
  Effect.sync(() => {
    entries.set(sdk.specifier, sdk.entry)
    if (registered) return
    registerHooks({
      resolve: (specifier, context, next) => {
        const entry = entries.get(specifier)
        if (entry !== undefined) return { url: entry, shortCircuit: true }
        const resolved = next(specifier, context)
        const carried = carriedOnto(context.parentURL, resolved.url)
        return carried === undefined ? resolved : { ...resolved, url: carried }
      }
    })
    registered = true
  })
