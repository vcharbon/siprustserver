/**
 * `siprig-in verb <verb>` runs a pack's command `siprig-<verb>`; the base's
 * own commands (`siprig-in`, `siprig-site`, `siprig-store`,
 * `siprig-workspace`, `siprig-busy`) are no verb: each is refused, exit 2,
 * naming it, before anything runs.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { KIT_DIR, tree } from "./kit-harness.ts"

const SIPRIG_IN = path.join(KIT_DIR, "image/bin/siprig-in")

describe("siprig-in verb", () => {
  for (const command of ["in", "site", "store", "workspace", "busy"]) {
    it(`refuses ${command}, a command of the image, as a verb`, () => {
      const ran = tree()
      const bin = tree({ [`siprig-${command}`]: { script: `#!/bin/sh\ntouch '${path.join(ran, "ran")}'\n` } })
      const out = spawnSync(SIPRIG_IN, ["verb", command], {
        encoding: "utf8",
        env: { ...process.env, PATH: `${bin}:${process.env.PATH ?? ""}` }
      })
      expect({ status: out.status, named: out.stderr.includes(`${command} is`), ran: fs.existsSync(path.join(ran, "ran")) })
        .toEqual({ status: 2, named: true, ran: false })
    })
  }
})
