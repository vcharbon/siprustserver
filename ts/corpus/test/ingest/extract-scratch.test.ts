/**
 * An extraction's scratch is the store's own: a fresh directory under the
 * store's `.scratch/`, never under the temp dir, and the raw flows document
 * inside it is named so no walk for flows documents can take it.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { rawFlowsName, SCRATCH, scratchFor } from "../../src/ingest/extract.js"

let root = ""
beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "extract-scratch-"))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const scratchOf = (artifacts: string) =>
  Effect.runPromise(
    Effect.flatMap(FileSystem.FileSystem, (files) => scratchFor(files, artifacts)).pipe(
      Effect.provide(NodeServices.layer)
    )
  )

describe("an extraction's scratch", () => {
  it("is a fresh directory under the store's own scratch, never under the temp dir", async () => {
    const artifacts = path.join(root, "store")
    const one = await scratchOf(artifacts)
    expect(path.dirname(one)).toBe(path.join(artifacts, SCRATCH))
    expect(fs.statSync(one).isDirectory()).toBe(true)
    expect(path.dirname(one)).not.toBe(os.tmpdir())
    expect(await scratchOf(artifacts)).not.toBe(one)
  })

  it("names the raw document so no walk for flows documents can take it", () => {
    expect(rawFlowsName("trace.pcap.gz").endsWith(".flows.json")).toBe(false)
  })
})
