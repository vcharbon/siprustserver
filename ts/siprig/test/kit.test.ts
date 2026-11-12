/**
 * The kit's front door (`tooling/siprig/kit pack`) refuses a pack manifest it
 * cannot honour before any build starts: docker is never called, the exit
 * status is 2 and stderr names the key. A manifest it can honour reaches one
 * `docker build` of the pack's image from the base image named.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"

const KIT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../tooling/siprig/kit")

/** A directory holding a `docker` that appends each of its command lines to `calls.log` there and succeeds. */
const fakeDocker = (): { readonly bin: string; readonly calls: () => ReadonlyArray<string> } => {
  const bin = fs.mkdtempSync(path.join(os.tmpdir(), "kit-docker-"))
  const log = path.join(bin, "calls.log")
  fs.writeFileSync(path.join(bin, "docker"), `#!/bin/sh\necho "$*" >> '${log}'\ncat > /dev/null\nexit 0\n`, { mode: 0o755 })
  return { bin, calls: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : []) }
}

/** A pack directory holding `manifest` as its pack.toml, and `files` beside it. */
const packDir = (manifest: string, files: Readonly<Record<string, string>> = {}): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kit-pack-"))
  fs.writeFileSync(path.join(dir, "pack.toml"), manifest)
  for (const [name, text] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, name)), { recursive: true })
    fs.writeFileSync(path.join(dir, name), text)
  }
  return dir
}

const MANIFEST = `schema = "pack.v1"\nname = "sample"\ntitle = "a sample pack"\nwrapper = "sample"\n`

/** `kit pack <dir>` with the fake docker first on PATH. */
const kitPack = (dir: string, docker: { readonly bin: string }) =>
  spawnSync(KIT, ["pack", dir, "--tag", "t1", "--base", "siprig:b1"], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${docker.bin}:${process.env.PATH ?? ""}` }
  })

describe("kit pack", () => {
  it("refuses an unknown key before docker is called, naming the key", () => {
    const docker = fakeDocker()
    const out = kitPack(packDir(`${MANIFEST}[image]\npython = "3.13"\n`), docker)
    expect({ status: out.status, named: out.stderr.includes("image.python: not a key of this file"), calls: docker.calls() })
      .toEqual({ status: 2, named: true, calls: [] })
  })

  it("refuses a missing binaries directory before docker is called, naming the key", () => {
    const docker = fakeDocker()
    const out = kitPack(packDir(`${MANIFEST}[image]\nbinaries = "./bin"\n`), docker)
    expect({ status: out.status, named: out.stderr.includes("image.binaries: the pack holds no directory ./bin"), calls: docker.calls() })
      .toEqual({ status: 2, named: true, calls: [] })
  })

  it("builds the pack's image, tagged <name>-rig:<tag>, from the base image named", () => {
    const docker = fakeDocker()
    const out = kitPack(packDir(MANIFEST), docker)
    const builds = docker.calls().filter((call) => call.startsWith("build "))
    expect({ status: out.status, builds: builds.length }).toEqual({ status: 0, builds: 1 })
    expect(builds[0]).toContain("--build-arg BASE=siprig:b1")
    expect(builds[0]).toContain("-t sample-rig:t1")
  })
})
