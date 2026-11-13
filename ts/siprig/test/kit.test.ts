/**
 * The kit's front door (`tooling/siprig/kit pack`). A pack manifest it cannot
 * honour is refused before any build starts, the pack read as the image
 * receives it (its checkout's tracked and untracked files, no ignored one):
 * docker is never called, the exit status is 2 and stderr names the key. A
 * manifest it can honour reaches one `docker build` of the pack's image from
 * the base image named, whose context holds the pack's tree, its binaries
 * with every link followed, its stamp and the wrapper written for the image.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { checkout, fakeDocker, filesUnder, git, kit, kitPack, lay, manifest, tree } from "./kit-harness.ts"

/** What a refused `kit pack` shows: its status, whether stderr names `key`, and docker's calls. */
const refusal = (out: { readonly status: number | null; readonly stderr: string }, key: string, calls: ReadonlyArray<string>) => ({
  status: out.status,
  named: out.stderr.includes(`${key}:`),
  calls
})

const REFUSED = { status: 2, named: true, calls: [] }

describe("kit pack refuses before docker is called, naming the key", () => {
  it("refuses an unknown key", () => {
    const docker = fakeDocker()
    const out = kitPack(tree({ "pack.toml": manifest(`[image]\npython = "3.13"`) }), docker)
    expect(refusal(out, "image.python", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a missing binaries directory", () => {
    const docker = fakeDocker()
    const out = kitPack(tree({ "pack.toml": manifest(`[image]\nbinaries = "./bin"`) }), docker)
    expect(refusal(out, "image.binaries", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a setup command its checkout ignores, which the image would not hold", () => {
    const docker = fakeDocker()
    const dir = checkout("main", {
      "pack.toml": manifest(`[image]\nsetup = "./image/setup"`),
      ".gitignore": "image/setup\n",
      "image/setup": { script: "#!/bin/sh\n" }
    })
    expect(refusal(kitPack(dir, docker), "image.setup", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a module that is a link out of the pack", () => {
    const docker = fakeDocker()
    const elsewhere = tree({ "deployment.ts": "export {}\n" })
    const dir = checkout("main", {
      "pack.toml": manifest(`[modules]\ndeployment = "./deployment.ts"`),
      "deployment.ts": { link: path.join(elsewhere, "deployment.ts") }
    })
    expect(refusal(kitPack(dir, docker), "modules.deployment", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a module inside a repository nested in the pack, whose files its checkout does not hold", () => {
    const docker = fakeDocker()
    const nested = { "nested/deployment.ts": "export {}\n" }
    const dir = tree({ "pack.toml": manifest(`[modules]\ndeployment = "./nested/deployment.ts"`), ...nested })
    git(path.join(dir, "nested"), "init", "-q", "-b", "main")
    git(path.join(dir, "nested"), "add", "-A")
    git(path.join(dir, "nested"), "commit", "-q", "-m", "nested")
    git(dir, "init", "-q", "-b", "main")
    git(dir, "add", "-A")
    git(dir, "commit", "-q", "-m", "pack")
    expect(refusal(kitPack(dir, docker), "modules.deployment", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a module that is a relative link out of the pack, naming where it leads", () => {
    const docker = fakeDocker()
    const parent = tree({
      "outside/deployment.ts": "export {}\n",
      "pack/pack.toml": manifest(`[modules]\ndeployment = "./deployment.ts"`),
      "pack/deployment.ts": { link: "../outside/deployment.ts" }
    })
    const dir = path.join(parent, "pack")
    git(dir, "init", "-q", "-b", "main")
    git(dir, "add", "-A")
    git(dir, "commit", "-q", "-m", "pack")
    const out = kitPack(dir, docker)
    expect({
      ...refusal(out, "modules.deployment", docker.calls()),
      leads: out.stderr.includes(`leads out of the pack, to ${fs.realpathSync(path.join(parent, "outside/deployment.ts"))}`),
      ignored: out.stderr.includes("ignored file")
    }, out.stderr).toEqual({ ...REFUSED, leads: true, ignored: false })
  })

  it("refuses a link to nothing among the binaries", () => {
    const docker = fakeDocker()
    const dir = tree({ "pack.toml": manifest(`[image]\nbinaries = "./bin"`), "bin/tool": { link: "/nonexistent/tool" } })
    expect(refusal(kitPack(dir, docker), "image.binaries", docker.calls())).toEqual(REFUSED)
  })

  it("refuses a link to nothing inside a directory the binaries link to, naming it", () => {
    const docker = fakeDocker()
    const linked = tree({ "tool": { link: "/nonexistent/tool" } })
    const dir = tree({ "pack.toml": manifest(`[image]\nbinaries = "./bin"`), "bin/lib": { link: linked } })
    const out = kitPack(dir, docker)
    expect({ ...refusal(out, "image.binaries", docker.calls()), named: out.stderr.includes("bin/lib/tool") }, out.stderr)
      .toEqual({ ...REFUSED, named: true })
  })

  it("prints its usage, not a shell error, for an option with no value", () => {
    const docker = fakeDocker()
    const out = kit(docker, "pack", tree({ "pack.toml": manifest() }), "--tag")
    expect({ status: out.status, usage: out.stderr.includes("kit pack <dir>"), unbound: out.stderr.includes("unbound") })
      .toEqual({ status: 2, usage: true, unbound: false })
  })
})

describe("kit pack builds", () => {
  /** A checkout on branch `it's-x` holding the sample pack with binaries, then an untracked and an ignored file beside. */
  const built = () => {
    const docker = fakeDocker()
    const elsewhere = tree({ "tool": { script: "#!/bin/sh\necho tool\n" } })
    const dir = checkout("it's-x", {
      "pack.toml": manifest(`[image]\nbinaries = "./build"`),
      ".gitignore": "build/\nignored.txt\n",
      "tracked.txt": "tracked\n"
    })
    lay(dir, {
      "untracked.txt": "untracked\n",
      "ignored.txt": "ignored\n",
      "build/own": { script: "#!/bin/sh\necho own\n" },
      "build/tool": { link: path.join(elsewhere, "tool") }
    })
    const out = kitPack(dir, docker)
    return { docker, dir, out, context: docker.context }
  }

  it("builds the pack's image, tagged <name>-rig:<tag>, from the base image named", () => {
    const { docker, out } = built()
    const builds = docker.calls().filter((call) => call.startsWith("build "))
    expect({ status: out.status, builds: builds.length }).toEqual({ status: 0, builds: 1 })
    expect(builds[0]).toContain("--build-arg BASE=siprig:b1")
    expect(builds[0]).toContain("-t sample-rig:t1")
  })

  it("stages the pack's tree as its checkout holds it: tracked and untracked files, no ignored one", () => {
    const { context } = built()
    expect(filesUnder(path.join(context, "pack"))).toEqual([".gitignore", "pack.toml", "tracked.txt", "untracked.txt"])
  })

  it("stages the binaries as files, a link followed to the file it names", () => {
    const { context } = built()
    const binaries = path.join(context, "binaries")
    const tool = fs.lstatSync(path.join(binaries, "tool"))
    expect({
      files: filesUnder(binaries),
      toolIsFile: tool.isFile(),
      toolRuns: (tool.mode & 0o111) !== 0,
      tool: fs.readFileSync(path.join(binaries, "tool"), "utf8")
    }).toEqual({ files: ["own", "tool"], toolIsFile: true, toolRuns: true, tool: "#!/bin/sh\necho tool\n" })
  })

  it("writes the wrapper naming the pack's wrapper and the image's tag", () => {
    const { context } = built()
    const wrapper = fs.readFileSync(path.join(context, "wrapper"), "utf8")
    expect({
      name: wrapper.includes("\nNAME=sample\n"),
      image: wrapper.includes(`IMAGE="\${SIPRIG_IMAGE:-sample-rig:t1}"`),
      placeholders: /@(WRAPPER|IMAGE)@/.test(wrapper)
    }).toEqual({ name: true, image: true, placeholders: false })
  })

  it("stamps the pack's provenance off its checkout, its branch byte for byte", () => {
    const { context, dir } = built()
    const stamp = fs.readFileSync(path.join(context, "stamp", "sample.env"), "utf8").split("\n")
    expect(stamp).toContain(`SHA=${git(dir, "rev-parse", "HEAD").trim()}`)
    expect(stamp).toContain("BRANCH=it's-x")
    expect(stamp).toContain("DIR=/opt/siprig/pack")
  })
})
