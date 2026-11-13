/**
 * The image's `git` (`tooling/siprig/image/bin/git`) answers a run's
 * provenance questions from the stamp whose `DIR` is the directory asked in.
 * A stamp is data: each value is answered byte for byte and never run, and a
 * stamp lacking a key the question needs is an error naming it.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { KIT_DIR } from "./kit-harness.ts"

const GIT = path.join(KIT_DIR, "image/bin/git")

/** A checkout directory and a stamp directory whose `sample.env` holds `lines` and `DIR=` that checkout. */
const stamped = (lines: ReadonlyArray<string>) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "image-git-checkout-"))
  const stamps = fs.mkdtempSync(path.join(os.tmpdir(), "image-git-stamps-"))
  fs.writeFileSync(path.join(stamps, "sample.env"), [...lines, `DIR=${fs.realpathSync(dir)}`, ""].join("\n"))
  return { dir, stamps }
}

/** The image's `git <args>` asked in `dir`, reading the stamps in `stamps`. */
const ask = (at: { readonly dir: string; readonly stamps: string }, ...args: ReadonlyArray<string>) => {
  const out = spawnSync(GIT, args, { cwd: at.dir, encoding: "utf8", env: { ...process.env, SIPRIG_STAMP_DIR: at.stamps } })
  return { status: out.status, stdout: out.stdout, stderr: out.stderr }
}

const SHA = "0123456789abcdef0123456789abcdef01234567"

describe("the image's git", () => {
  it("answers a branch holding shell syntax as it stands, never running it", () => {
    const at = stamped([`SHA=${SHA}`, "BRANCH=fix;echo INJECTED", "DIRTY=0", "DIFF="])
    expect(ask(at, "rev-parse", "--abbrev-ref", "HEAD")).toEqual({ status: 0, stdout: "fix;echo INJECTED\n", stderr: "" })
  })

  it("answers a branch holding a quote", () => {
    const at = stamped([`SHA=${SHA}`, "BRANCH=it's-x", "DIRTY=0", "DIFF="])
    expect(ask(at, "rev-parse", "--abbrev-ref", "HEAD")).toEqual({ status: 0, stdout: "it's-x\n", stderr: "" })
  })

  it("answers the stamped commit and a dirty build's diff digest", () => {
    const at = stamped([`SHA=${SHA}`, "BRANCH=main", "DIRTY=1", "DIFF=feed"])
    expect([ask(at, "rev-parse", "HEAD").stdout, ask(at, "diff", "HEAD").stdout]).toEqual([`${SHA}\n`, "feed\n"])
  })

  it("fails naming the key a stamp lacks", () => {
    const at = stamped([`SHA=${SHA}`, "DIRTY=0", "DIFF="])
    const out = ask(at, "rev-parse", "--abbrev-ref", "HEAD")
    expect({ failed: out.status !== 0, named: out.stderr.includes("BRANCH"), stdout: out.stdout })
      .toEqual({ failed: true, named: true, stdout: "" })
  })
})
