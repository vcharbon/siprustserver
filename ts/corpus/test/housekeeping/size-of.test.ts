/**
 * What a retention pass measures of a bundle is the bytes the bundle itself
 * holds: a link inside it to a file or a directory elsewhere frees nothing
 * when the bundle goes, so it counts for nothing.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Retention from "../../src/housekeeping/retention.js"

const root = fs.mkdtempSync(path.join(os.tmpdir(), "retention-size-"))
afterAll(() => fs.rmSync(root, { recursive: true, force: true }))

describe("Retention.sizeOf", () => {
  it("counts the files under a bundle and none a link inside it points at", async () => {
    const bundle = path.join(root, "run1")
    const elsewhere = path.join(root, "elsewhere")
    fs.mkdirSync(path.join(bundle, "cell"), { recursive: true })
    fs.mkdirSync(elsewhere)
    fs.writeFileSync(path.join(bundle, "cell", "verdict.json"), "x".repeat(100))
    fs.writeFileSync(path.join(elsewhere, "big.bin"), "y".repeat(10_000))
    fs.symlinkSync(path.join(elsewhere, "big.bin"), path.join(bundle, "linked-file"))
    fs.symlinkSync(elsewhere, path.join(bundle, "linked-dir"))
    const size = await Effect.runPromise(Retention.sizeOf(bundle).pipe(Effect.provide(NodeServices.layer)))
    expect(size).toBe(100)
  })
})
