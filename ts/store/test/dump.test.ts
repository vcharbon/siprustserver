/**
 * The backup's `pg_dump`, with no server: the password goes to the child in
 * `PGPASSWORD`, never on its command line, and the dump lands under its name
 * only once the child is done.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as AutoMigrate from "../src/auto-migrate.js"

let dir: string
let before: string | undefined
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "store-schema-dump-"))
  before = process.env.PATH
})
afterEach(() => {
  process.env.PATH = before
  fs.rmSync(dir, { recursive: true, force: true })
})

describe("the dump's credentials", () => {
  it("hands pg_dump the password in PGPASSWORD, never on its command line", async () => {
    const log = path.join(dir, "log")
    fs.writeFileSync(
      path.join(dir, "pg_dump"),
      `#!/bin/sh\necho "argv: $*" >>'${log}'\necho "password: $PGPASSWORD" >>'${log}'\n` +
        `for a in "$@"; do case "$a" in --file=*) : >"\${a#--file=}";; esac; done\n`,
      { mode: 0o755 }
    )
    process.env.PATH = `${dir}:${before ?? ""}`
    const file = path.join(dir, "backups", "x.dump")
    const written = await Effect.runPromise(
      AutoMigrate.dump("postgresql://rig:s3cr%40t@127.0.0.1:5499/run_store", file).pipe(
        Effect.provide(NodeServices.layer)
      )
    )
    expect(written).toBe(file)
    expect(fs.readdirSync(path.dirname(file))).toEqual(["x.dump"])
    const lines = fs.readFileSync(log, "utf8")
    expect(lines).toContain("argv: --format=custom")
    expect(lines).toContain("--dbname=postgresql://rig@127.0.0.1:5499/run_store")
    expect(lines).not.toMatch(/argv:.*s3cr/)
    expect(lines).toContain("password: s3cr@t")
  })

  it("leaves a url without a password as it is", () => {
    const url = "postgresql:///run_store?host=/var/run/postgresql&port=5444"
    expect(AutoMigrate.credentialsOf(url)).toEqual({ url, password: undefined })
  })
})
