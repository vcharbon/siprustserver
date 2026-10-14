/**
 * A phase's fingerprint is the content of what the deployment says decides
 * it. A process reads each once per set of inputs, so two deployments in one
 * process never share a hash; a long-lived reader asks for it now and sees
 * every edit.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
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

  it("read now, follows an edit to what decides it", async () => {
    const before = await under(roots[0]!, Fingerprint.extractorNow)
    fs.writeFileSync(path.join(roots[0]!, "extract.rs"), "fn extract() { edited }\n")
    const after = await under(roots[0]!, Fingerprint.extractorNow)
    expect(after).not.toBe(before)
    expect(after).toBe(await under(roots[0]!, Fingerprint.contentHash(roots[0]!, ["extract.rs"])))
  })
})
