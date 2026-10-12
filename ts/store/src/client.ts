/**
 * The connection to the store's postgres database.
 *
 * `DATABASE_URL` names it; without it the default is a local server reached
 * over its unix socket, so no secret is needed. A deployment with a server of
 * its own hands its url to {@link layerFromUrl}.
 */
import * as Config from "effect/Config"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Redacted from "effect/Redacted"
import { PgClient } from "@effect/sql-pg"
import * as crypto from "node:crypto"
import * as fs from "node:fs"
import * as path from "node:path"

/** A local server's `sip_store` database, over the unix socket. */
export const DEFAULT_URL = "postgresql:///sip_store?host=/var/run/postgresql"

export const url = Config.redacted("DATABASE_URL").pipe(
  Config.withDefault(Redacted.make(DEFAULT_URL))
)

export const layerFromUrl = (value: string) => PgClient.layer({ url: Redacted.make(value) })

/** The default wiring: `DATABASE_URL`, else {@link DEFAULT_URL}. */
export const layer = Layer.unwrap(Effect.map(url, (value) => PgClient.layer({ url: value })))

/** The same server's database `database`: `url` with its database name replaced. */
export const atDatabase = (value: string, database: string): string => {
  const parsed = new URL(value)
  parsed.pathname = `/${database}`
  return parsed.toString()
}

/** The directory holding `.git` at or above `from`: the checkout, else `from` itself. */
const checkoutOf = (from: string): string => {
  let dir = path.resolve(from)
  for (;;) {
    if (fs.existsSync(path.join(dir, ".git"))) return dir
    const up = path.dirname(dir)
    if (up === dir) return path.resolve(from)
    dir = up
  }
}

/**
 * A test suite's own database name, distinct per checkout:
 * `<base>_<checkout name>_<hash of its path>`, the checkout being the directory
 * holding `.git` above the process, reduced to what postgres takes unquoted
 * and to its 63 characters. Two worktrees running one suite at once build and
 * drop two databases, never each other's, even where their directories share
 * a name.
 */
export const scratchDatabase = (base: string, cwd: string = process.cwd()): string => {
  const checkout = checkoutOf(cwd)
  const name = path.basename(checkout).toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "")
  const hash = crypto.createHash("sha256").update(checkout).digest("hex").slice(0, 8)
  const head = `${base}_${name}`.slice(0, 63 - hash.length - 1).replace(/_+$/, "")
  return `${head}_${hash}`
}
