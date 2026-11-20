/**
 * The identity extraction, end to end through the ingest with a stand-in
 * extractor: the kept document is the raw one byte for byte and no seed is
 * kept; extracting a settled capture again keeps its name while its bytes
 * stand, and lands under the next generation once they changed (in place
 * where nothing settles the name); a failed extraction reads back as its
 * line, and is refused at the cut under one neutral token, the line verbatim.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Extraction, ExtractionIdentity, Tools } from "../../src/deployment/index.js"
import type { Source } from "../../src/ingest/discover.js"
import * as Extract from "../../src/ingest/extract.js"
import * as Record from "../../src/ingest/record.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "extraction-identity-test-"))
afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

/** A stand-in extractor: the raw document is the pcap's bytes; an index is a marker file. */
const EXTRACTOR = path.join(scratch, "extractor")
fs.writeFileSync(EXTRACTOR, "#!/bin/sh\nif [ \"$1\" = index ]; then : > \"$2.idx\"; exit 0; fi\ncat \"$1\"\n", { mode: 0o755 })

const tools = Tools.layerWith({
  ready: () => Effect.succeed(EXTRACTOR),
  generator: Effect.die("unused"),
  measure: Effect.die("unused"),
  compileMs: Effect.succeed(0),
  decides: Effect.die("unused"),
  reading: []
})

const artifacts = path.join(scratch, "store")

const extract = (source: Source, settled: Extract.Settled = Extract.EVERY_NAME_SETTLED) =>
  Effect.runPromise(
    Extract.extract(artifacts, source, "extractor-v1", EXTRACTOR, undefined, settled).pipe(
      Effect.provide(Layer.merge(tools, identity))
    )
  )

const identity = ExtractionIdentity.layer.pipe(Layer.provideMerge(NodeServices.layer))

const withIdentity = <A, E>(effect: Effect.Effect<A, E, Extraction.Service>) =>
  Effect.runPromise(effect.pipe(Effect.provide(identity)))

/** A pcap of `bytes` under `name`, and the source naming it. */
const sourceOf = (name: string, bytes: string): Source => {
  const file = path.join(scratch, "pcaps", `${name}.pcapng`)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, bytes)
  return { capture: name, pcaps: [file] }
}

const documentOf = (capture: string): string =>
  fs.readFileSync(path.join(Record.captureDir(artifacts, capture), ExtractionIdentity.documentName(capture)), "utf8")

describe("the identity extraction", () => {
  it("keeps the raw document as it is, under no seed", async () => {
    const done = await extract(sourceOf("one", "raw flows"))
    expect([done.capture, done.broke]).toEqual(["one", undefined])
    expect(documentOf("one")).toBe("raw flows")
    expect((await Effect.runPromise(Record.read(artifacts, "one").pipe(Effect.provide(NodeServices.layer))))?.seedFile)
      .toBeUndefined()
  })

  it("keeps a settled capture's name when it is extracted again and its bytes stand", async () => {
    const source = sourceOf("two", "raw flows")
    await extract(source)
    const again = await extract(source)
    expect([again.capture, again.broke]).toEqual(["two", undefined])
    expect(fs.existsSync(Record.captureDir(artifacts, "two.anon2")), "no next generation").toBe(false)
  })

  it("lands a settled capture whose bytes changed under its next generation, and an unsettled one in place", async () => {
    await extract(sourceOf("three", "raw flows"))
    const settled = await extract(sourceOf("three", "other flows"))
    expect([settled.capture, documentOf(settled.capture)]).toEqual(["three.anon2", "other flows"])
    expect(documentOf("three"), "the old name as it was").toBe("raw flows")
    await extract(sourceOf("four", "raw flows"), () => false)
    const open = await extract(sourceOf("four", "other flows"), () => false)
    expect([open.capture, documentOf("four")]).toEqual(["four", "other flows"])
  })

  it("reads a failed extraction back as its line, refused at the cut under one token with the line verbatim", async () => {
    const dir = path.join(scratch, "failed")
    fs.mkdirSync(dir, { recursive: true })
    expect(await withIdentity(Effect.flatMap(Extraction.Service, (one) => one.failureOf(dir, "five")))).toBeUndefined()
    await withIdentity(Effect.flatMap(Extraction.Service, (one) => one.recordFailure(dir, "five", "sipflow failed: truncated")))
    expect(await withIdentity(Effect.flatMap(Extraction.Service, (one) => one.failureOf(dir, "five")))).toBe(
      "sipflow failed: truncated"
    )
    const refusal = await withIdentity(
      Effect.flatMap(Extraction.Service, (one) => one.sourceRefusal({ capture: "five", error: "sipflow failed: truncated" }))
    )
    expect(refusal).toMatchObject({
      caseId: "five-source",
      reason: ExtractionIdentity.SOURCE_REFUSED,
      legs: [],
      callIds: [],
      evidence: [{ detail: "sipflow failed: truncated" }]
    })
    expect(refusal.line).toContain("sipflow failed: truncated")
  })
})
