/**
 * The recordings a confrontation reads come back keyed by leg name in name
 * order, whatever order the directory lists them in: that order breaks the tie
 * between two receptions of the same microsecond on two legs.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { readRecordings } from "../src/recordings.js"

describe("readRecordings", () => {
  it("keys the legs by name, in name order, and skips what is no recording", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "recordings-"))
    fs.mkdirSync(path.join(dir, "recording"))
    for (const name of ["C.jsonl", "A.jsonl", "notes.txt", "B.jsonl", "AA.jsonl"]) {
      fs.writeFileSync(path.join(dir, "recording", name), "")
    }
    const got = await Effect.runPromise(readRecordings(dir).pipe(Effect.provide(NodeServices.layer)))
    expect([...got.keys()]).toEqual(["A", "AA", "B", "C"])
    fs.rmSync(dir, { recursive: true, force: true })
  })

  it("reads a bundle with no recording as no legs", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "recordings-"))
    const got = await Effect.runPromise(readRecordings(dir).pipe(Effect.provide(NodeServices.layer)))
    expect(got.size).toBe(0)
    fs.rmSync(dir, { recursive: true, force: true })
  })
})
