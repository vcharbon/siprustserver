/**
 * The wrapper the kit writes for a pack (`tooling/siprig/wrapper`, rendered by
 * `kit pack`): its usage is its header, an option given no value prints that
 * usage, and `up` refuses a workspace another container already runs over
 * (the label `org.siprig.workspace`), whichever wrapper started it.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { beforeAll, describe, expect, it } from "vitest"
import { type Docker, fakeDocker, kitPack, manifest, tree } from "./kit-harness.ts"

/** The wrapper `kit pack` wrote for the sample pack, copied out of the build's context. */
let wrapper = ""

beforeAll(() => {
  const docker = fakeDocker()
  const out = kitPack(tree({ "pack.toml": manifest() }), docker)
  if (out.status !== 0) throw new Error(`kit pack: ${out.stderr}`)
  wrapper = path.join(tree(), "sample")
  fs.copyFileSync(path.join(docker.context, "wrapper"), wrapper)
  fs.chmodSync(wrapper, 0o755)
})

/** `sample <args>` with `docker` first on PATH. */
const run = (docker: Docker, ...args: ReadonlyArray<string>) =>
  spawnSync(wrapper, args, { encoding: "utf8", env: { ...process.env, PATH: `${docker.bin}:${process.env.PATH ?? ""}` } })

/** Whether `stderr` is the wrapper's usage: its header's first line, and nothing of the script below it. */
const isUsage = (stderr: string): boolean =>
  stderr.includes("sample — a siprig pack's image on this machine") && !stderr.includes("set -euo pipefail") &&
  !stderr.includes("unbound variable")

describe("the wrapper's usage", () => {
  it("is its header alone", () => {
    const out = run(fakeDocker(), "--help")
    expect({ status: out.status, usage: isUsage(out.stderr), last: out.stderr.trimEnd().split("\n").at(-1) })
      .toEqual({ status: 2, usage: true, last: "The image is SIPRIG_IMAGE, else sample-rig:t1, the one the kit wrote this wrapper for." })
  })

  for (const args of [["-w"], ["up", "--compose"], ["up", "--memory"], ["upgrade-pg", "--from"]]) {
    it(`is printed for \`${args.join(" ")}\` with no value`, () => {
      const workspace = tree({ "rig.toml": "" })
      const out = run(fakeDocker(), ...(args[0] === "-w" ? args : ["-w", workspace, ...args]))
      expect({ status: out.status, usage: isUsage(out.stderr) }).toEqual({ status: 2, usage: true })
    })
  }
})

describe("the wrapper's up", () => {
  it("refuses a workspace another container runs over, naming it, and starts none", () => {
    const docker = fakeDocker()
    docker.holders(["other-ws-1234"])
    const out = run(docker, "-w", tree({ "rig.toml": "" }), "up")
    expect({
      status: out.status,
      named: out.stderr.includes("other-ws-1234"),
      runs: docker.calls().filter((call) => call.startsWith("run ")).length
    }).toEqual({ status: 1, named: true, runs: 0 })
  })

  it("starts its container where no other runs over the workspace", () => {
    const docker = fakeDocker()
    run(docker, "-w", tree({ "rig.toml": "" }), "up")
    expect(docker.calls().filter((call) => call.startsWith("run ")).length).toEqual(1)
  })
})
