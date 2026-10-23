/**
 * The session tree root: the ONE directory every stored bundle path is
 * relative to.
 *
 * A run row names its bundle as `<stem>/run<id>` — where the bundle stands
 * INSIDE the tree — never where the tree stands on a box, so the same database
 * answers under any checkout and moving the tree is a config line, not a
 * rewrite. The services that read or write bundle paths (`runs.ts`,
 * `rulehits.ts`) take the root from here and hand callers absolute paths;
 * nothing outside this package sees a stored form.
 */
import * as Context from "effect/Context"
import * as Layer from "effect/Layer"
import * as path from "node:path"

export interface Root {
  /** The session tree, absolute. */
  readonly dir: string
}

export class Service extends Context.Service<Service, Root>()("@sip/store/Sessions") {}

export const layer = (dir: string): Layer.Layer<Service> =>
  Layer.succeed(Service, { dir: path.resolve(dir) })

/** The stored form of a bundle under the tree; a path outside it is a defect, not a row. */
export const toStored = (root: Root, absolute: string): string => {
  const stored = path.relative(root.dir, path.resolve(absolute))
  if (stored === "" || stored.startsWith("..") || path.isAbsolute(stored)) {
    throw new Error(`bundle ${absolute} is not under the session tree ${root.dir}`)
  }
  return stored
}

/** The bundle a row names, on this box. */
export const fromStored = (root: Root, stored: string): string => path.join(root.dir, stored)
