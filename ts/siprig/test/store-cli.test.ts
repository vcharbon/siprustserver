/**
 * `siprig-store migrate` migrates the store the workspace names: with no
 * `DATABASE_URL` exported, the one `rig.toml` states as `[database] url`, a
 * server other than the container's own. DB-bound: it creates and drops its
 * own database on the test server (`@sip/store/test-server`).
 */
import * as TestServer from "@sip/store/test-server"
import { spawnSync } from "node:child_process"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { tree } from "./kit-harness.ts"

const PACKAGE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const TSX = path.join(PACKAGE, "node_modules/.bin/tsx")
const DATABASE = `siprig_store_cli_${process.pid}`

const drop = () => TestServer.on("postgres", TestServer.dropDatabase(DATABASE))

describe.skipIf(!TestServer.served)("siprig-store migrate", () => {
  beforeAll(drop)
  afterAll(drop)

  it("creates and migrates the database rig.toml's [database] url names, no DATABASE_URL exported", () => {
    const workspace = tree({ "rig.toml": `[database]\nurl = "${TestServer.urlFor(DATABASE)}"\n` })
    const env: NodeJS.ProcessEnv = { ...process.env, SIPRIG_WORKSPACE: workspace }
    for (const name of ["DATABASE_URL", "PGHOST", "PGPORT", "PGDATABASE"]) delete env[name]
    const out = spawnSync(TSX, [path.join(PACKAGE, "src/store-cli.ts"), "migrate"], { cwd: PACKAGE, encoding: "utf8", env })
    expect({ status: out.status, created: out.stdout.includes(`[store] created database ${DATABASE}`) }, out.stderr)
      .toEqual({ status: 0, created: true })
  })
})
