/**
 * The wrapper the kit writes for a pack (`tooling/siprig/wrapper`, rendered by
 * `kit pack`): its usage is its header, an option given no value prints that
 * usage, `up` refuses a workspace another container already runs over (the
 * label `org.siprig.workspace`), whichever wrapper started it, and reports a
 * container its site stopped by exiting before it ever answered with the
 * tail of the site's log and the log's path. The kit's renderer
 * (`render-wrapper`) writes the very wrapper `kit pack` puts beside the image.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { beforeAll, describe, expect, it } from "vitest"
import { type Docker, fakeDocker, KIT_DIR, kitPack, manifest, tree } from "./kit-harness.ts"

/** The container's exit status when its site exited before it ever answered (`image/site-finish`). */
const SITE_NEVER_ANSWERED = 3

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

  it("reports a site that exited before it ever answered: the tail of its log, and the log's path", () => {
    const docker = fakeDocker()
    docker.stopped(SITE_NEVER_ANSWERED, "s6-rc: info: service site: starting\n")
    const workspace = tree({ "rig.toml": "", "logs/site/current": "@4000 site: listening\n@4001 Error: the pack's lane x is refused\n" })
    const out = run(docker, "-w", workspace, "up")
    expect({
      status: out.status,
      container: out.stderr.includes("s6-rc: info: service site: starting"),
      tail: out.stderr.includes("@4001 Error: the pack's lane x is refused"),
      named: out.stderr.includes(`the site exited before it ever answered: its log is ${workspace}/logs/site/current`)
    }, out.stderr).toEqual({ status: 1, container: true, tail: true, named: true })
  })

  it("prints no site log for a container that stopped for another reason", () => {
    const docker = fakeDocker()
    docker.stopped(1, "siprig: rig.toml: [site] port is no port\n")
    const workspace = tree({ "rig.toml": "", "logs/site/current": "@4000 an earlier start's line\n" })
    const out = run(docker, "-w", workspace, "up")
    expect({
      status: out.status,
      container: out.stderr.includes("[site] port is no port"),
      tail: out.stderr.includes("an earlier start's line")
    }, out.stderr).toEqual({ status: 1, container: true, tail: false })
  })
})

describe("the kit's wrapper renderer", () => {
  it("names the pack's wrapper and image and leaves no placeholder", () => {
    const out = spawnSync(path.join(KIT_DIR, "render-wrapper"), ["sample", "sample-rig:t1"], { encoding: "utf8" })
    expect({
      status: out.status,
      name: out.stdout.includes("\nNAME=sample\n"),
      image: out.stdout.includes(`IMAGE="\${SIPRIG_IMAGE:-sample-rig:t1}"`),
      placeholders: /@[A-Z]+@/.test(out.stdout)
    }, out.stderr).toEqual({ status: 0, name: true, image: true, placeholders: false })
  })

  it("is what kit pack writes beside the image", () => {
    const docker = fakeDocker()
    const out = kitPack(tree({ "pack.toml": manifest() }), docker)
    const rendered = spawnSync(path.join(KIT_DIR, "render-wrapper"), ["sample", "sample-rig:t1"], { encoding: "utf8" })
    expect({ status: out.status, same: fs.readFileSync(path.join(docker.context, "wrapper"), "utf8") === rendered.stdout })
      .toEqual({ status: 0, same: true })
  })
})
