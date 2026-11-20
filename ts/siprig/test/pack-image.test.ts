/**
 * The image kit reads a pack's manifest before any build: a manifest it can
 * build from answers the image's facts, and one it cannot honour is refused,
 * naming the manifest and the dotted key at fault.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as PackImage from "../src/pack-image.ts"

/** One file to lay down: its text, or an executable script. */
type Entry = string | { readonly script: string }

/** `files` written under a new temporary directory, which is returned. */
const tree = (files: Readonly<Record<string, Entry>>): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pack-image-test-"))
  for (const [relative, entry] of Object.entries(files)) {
    const file = path.join(root, relative)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    if (typeof entry === "string") fs.writeFileSync(file, entry, { mode: 0o644 })
    else fs.writeFileSync(file, entry.script, { mode: 0o755 })
  }
  return root
}

/** A pack manifest naming `name`, its lines `extra` appended. */
const manifest = (extra = ""): string =>
  `schema = "pack.v1"\nname = "sample"\ntitle = "a sample pack"\nwrapper = "sample"\n${extra}\n`

/** The kit's reading of the pack at `dir`: its facts, or the refusal's message. */
const read = (dir: string) =>
  Effect.runPromise(
    PackImage.read(dir).pipe(
      Effect.map((facts) => ({ facts })),
      Effect.catch((refusal) => Effect.succeed({ refused: refusal.message })),
      Effect.provide(NodeServices.layer)
    )
  )

describe("a pack the kit builds", () => {
  it("answers the image's facts: its repository, wrapper, component, packages, binaries and setup", async () => {
    const dir = tree({
      "pack.toml": manifest(`component = "sample-product"\n[image]\npackages = ["jq", "libpcap0.8"]\nbinaries = "./bin"\nsetup = "./image/setup"`),
      "bin/replay": { script: "#!/bin/sh\n" },
      "image/setup": { script: "#!/bin/sh\n" }
    })
    expect(await read(dir)).toEqual({
      facts: {
        name: "sample",
        image: "sample-rig",
        wrapper: "sample",
        component: "sample-product",
        packages: ["jq", "libpcap0.8"],
        binaries: path.join(dir, "bin"),
        setup: "image/setup"
      }
    })
  })

  it("takes the pack's name as its component, no package, no binary and no setup where the manifest states none", async () => {
    const dir = tree({ "pack.toml": manifest() })
    expect(await read(dir)).toEqual({
      facts: { name: "sample", image: "sample-rig", wrapper: "sample", component: "sample", packages: [], binaries: undefined, setup: undefined }
    })
  })
})

describe("a pack the kit cannot honour", () => {
  it("is refused where the directory holds no pack.toml", async () => {
    const dir = tree({ "README.md": "not a pack" })
    expect(await read(dir)).toEqual({ refused: `${dir}: a pack holds pack.toml at its root` })
  })

  it("is refused naming a key the manifest's schema does not name", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\npython = "3.13"`) })
    expect(await read(dir)).toEqual({ refused: `${path.join(dir, "pack.toml")}: image.python: not a key of this file` })
  })

  it("is refused naming the key of a missing binaries directory", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\nbinaries = "./bin"`) })
    expect(await read(dir)).toEqual({ refused: `${path.join(dir, "pack.toml")}: image.binaries: the pack holds no directory ./bin` })
  })

  it("is refused where the binaries path is a file", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\nbinaries = "./bin"`), "bin": "a file" })
    expect(await read(dir)).toEqual({ refused: `${path.join(dir, "pack.toml")}: image.binaries: the pack holds no directory ./bin` })
  })

  it("is refused naming the key of a missing setup command", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\nsetup = "./image/setup"`) })
    expect(await read(dir)).toEqual({ refused: `${path.join(dir, "pack.toml")}: image.setup: the pack holds no executable ./image/setup` })
  })

  it("is refused where the setup command is not executable", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\nsetup = "./image/setup"`), "image/setup": "#!/bin/sh\n" })
    expect(await read(dir)).toEqual({ refused: `${path.join(dir, "pack.toml")}: image.setup: the pack holds no executable ./image/setup` })
  })

  it("is refused naming the package that is no Debian package name", async () => {
    const dir = tree({ "pack.toml": manifest(`[image]\npackages = ["jq", "jq; rm -rf /"]`) })
    expect(await read(dir)).toEqual({
      refused: `${path.join(dir, "pack.toml")}: image.packages[1]: "jq; rm -rf /" is no Debian package name`
    })
  })

  it("is refused naming the key of a missing path the rig reads", async () => {
    const dir = tree({ "pack.toml": manifest(`[modules]\ndeployment = "./deployment.ts"`) })
    expect(await read(dir)).toEqual({
      refused: `${path.join(dir, "pack.toml")}: modules.deployment: the pack holds no file ./deployment.ts`
    })
  })
})
