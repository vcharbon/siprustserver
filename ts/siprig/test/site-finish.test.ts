/**
 * The site's service scripts (`tooling/siprig/image/site-check`, `site-finish`):
 * the check marks the site answered once it answers; the finish of a site
 * that never answered sets the container's exit status 3, names the site's
 * log and halts the container, and the finish of a site that has answered
 * leaves it to s6 to restart.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { KIT_DIR, lay, tree } from "./kit-harness.ts"

const SITE_CHECK = path.join(KIT_DIR, "image/site-check")
const SITE_FINISH = path.join(KIT_DIR, "image/site-finish")

/** A container's run directory, exit code file and halt, as the finish sees them. */
const container = (answered: boolean) => {
  const dir = tree(answered ? { "run/site-answered": "" } : { "run/.keep": "" })
  lay(dir, { "halt": { script: `#!/bin/sh\n: > '${dir}/halted'\n` } })
  return {
    dir,
    workspace: tree(),
    run: path.join(dir, "run"),
    exitcode: path.join(dir, "exitcode"),
    halt: path.join(dir, "halt"),
    halted: () => fs.existsSync(path.join(dir, "halted")),
    status: () => (fs.existsSync(path.join(dir, "exitcode")) ? fs.readFileSync(path.join(dir, "exitcode"), "utf8") : undefined)
  }
}

/** `site-finish <args>` in `at`: its status, stderr, the container's exit code written, and whether it halted. */
const finish = (at: ReturnType<typeof container>, ...args: ReadonlyArray<string>) => {
  const out = spawnSync(SITE_FINISH, args, {
    encoding: "utf8",
    env: {
      ...process.env,
      SIPRIG_WORKSPACE: at.workspace,
      SIPRIG_RUN_DIR: at.run,
      SIPRIG_EXITCODE_FILE: at.exitcode,
      SIPRIG_HALT: at.halt
    }
  })
  return { status: out.status, stderr: out.stderr, exitcode: at.status(), halted: at.halted() }
}

/** `site-check` with a `siprig-in` whose `site-url` exits `answer`: its status, and whether it marked the site answered. */
const check = (answer: number) => {
  const run = tree()
  const bin = tree({ "siprig-in": { script: `#!/bin/sh\n[ "$1" = site-url ] && exit ${answer}\nexit 9\n` } })
  const out = spawnSync(SITE_CHECK, [], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH ?? ""}`, SIPRIG_RUN_DIR: run }
  })
  return { status: out.status, answered: fs.existsSync(path.join(run, "site-answered")) }
}

describe("the site's answer check", () => {
  it("marks the site answered once it answers", () => {
    expect(check(0)).toEqual({ status: 0, answered: true })
  })

  it("fails, marking nothing, while the site does not answer", () => {
    expect(check(1)).toEqual({ status: 1, answered: false })
  })
})

describe("the site's finish", () => {
  it("stops the container with status 3 where the site exited before it ever answered, naming its log", () => {
    const at = container(false)
    const out = finish(at, "1", "0")
    expect({
      status: out.status,
      exitcode: out.exitcode,
      halted: out.halted,
      named: out.stderr.includes(`its log is ${at.workspace}/logs/site/current`)
    }, out.stderr).toEqual({ status: 0, exitcode: "3\n", halted: true, named: true })
  })

  it("names the exit code of a site that exited, and no signal", () => {
    const out = finish(container(false), "1", "0")
    expect({ code: out.stderr.includes("exited with code 1"), signal: out.stderr.includes("signal") }, out.stderr)
      .toEqual({ code: true, signal: false })
  })

  it("names the signal that killed a site, and no exit code", () => {
    const out = finish(container(false), "256", "9")
    expect({ signal: out.stderr.includes("was killed by signal 9"), code: out.stderr.includes("code") }, out.stderr)
      .toEqual({ signal: true, code: false })
  })

  it("leaves a site that has answered to s6 to restart: no exit status written, no halt", () => {
    const out = finish(container(true), "1", "0")
    expect({ status: out.status, exitcode: out.exitcode, halted: out.halted })
      .toEqual({ status: 0, exitcode: undefined, halted: false })
  })
})
