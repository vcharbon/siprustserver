/**
 * A phase's fingerprint is the content of what the deployment says decides
 * it. A process reads each once per set of inputs, so two deployments in one
 * process never share a hash; a long-lived reader asks for it now and sees
 * every edit.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as FileSystemTag from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Tools } from "../src/deployment/index.js"
import * as Fingerprint from "../src/fingerprint.js"

const roots = [0, 1].map(() => fs.mkdtempSync(path.join(os.tmpdir(), "corpus-fingerprint-")))
roots.forEach((root, at) => fs.writeFileSync(path.join(root, "extract.rs"), `fn extract() { ${at} }\n`))

afterAll(() => {
  for (const root of roots) fs.rmSync(root, { recursive: true, force: true })
})

const deciding = (root: string) =>
  Tools.layerWith({
    ready: () => Effect.die("unused"),
    generator: Effect.die("unused"),
    measure: Effect.die("unused"),
    compileMs: Effect.succeed(0),
    decides: Effect.succeed({ root, extraction: ["extract.rs"], cut: [] })
  })

const under = <A, E>(root: string, effect: Effect.Effect<A, E, Tools.Service | NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.merge(deciding(root), NodeServices.layer))))

describe("Fingerprint.extractor", () => {
  it("is the hash of what each deployment states, never another's read earlier in the process", async () => {
    const first = await under(roots[0]!, Fingerprint.extractor)
    const second = await under(roots[1]!, Fingerprint.extractor)
    expect(second).not.toBe(first)
    expect(second).toBe(await under(roots[1]!, Fingerprint.contentHash(roots[1]!, ["extract.rs"])))
  })

  it("read now again over an unchanged tree, reads no file; an edited file is read again", async () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "corpus-fingerprint-reads-"))
    for (const name of ["a.rs", "b.rs", "c.rs"]) fs.writeFileSync(path.join(root, name), `fn ${name.slice(0, 1)}() {}\n`)
    const read: Array<string> = []
    const counting = Layer.effect(
      FileSystemTag.FileSystem,
      Effect.map(FileSystemTag.FileSystem, (real) => ({
        ...real,
        readFile: (file: string) => Effect.sync(() => void read.push(file)).pipe(Effect.andThen(real.readFile(file)))
      }))
    ).pipe(Layer.provide(NodeServices.layer))
    const tools = Tools.layerWith({
      ready: () => Effect.die("unused"),
      generator: Effect.die("unused"),
      measure: Effect.die("unused"),
      compileMs: Effect.succeed(0),
      decides: Effect.succeed({ root, extraction: ["a.rs", "b.rs", "c.rs"], cut: [] })
    })
    const now = () => Effect.runPromise(Fingerprint.extractorNow.pipe(Effect.provide(Layer.merge(tools, counting))))
    try {
      const first = await now()
      read.length = 0
      expect(await now()).toBe(first)
      expect(read).toEqual([])
      fs.writeFileSync(path.join(root, "b.rs"), "fn b() { edited }\n")
      expect(await now()).not.toBe(first)
      expect(read).toEqual([path.join(root, "b.rs")])
    } finally {
      fs.rmSync(root, { recursive: true, force: true })
    }
  })

  it("read now, follows an edit to what decides it", async () => {
    const before = await under(roots[0]!, Fingerprint.extractorNow)
    fs.writeFileSync(path.join(roots[0]!, "extract.rs"), "fn extract() { edited }\n")
    const after = await under(roots[0]!, Fingerprint.extractorNow)
    expect(after).not.toBe(before)
    expect(after).toBe(await under(roots[0]!, Fingerprint.contentHash(roots[0]!, ["extract.rs"])))
  })
})
