/**
 * Whether the artifacts on disk are still current, read off the store's
 * ingest record, and what a failed extraction left: the line the deployment's
 * anonymiser recorded, with the size of every file the record holds.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as crypto from "node:crypto"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Extraction } from "../../src/deployment/index.js"
import * as Extract from "../../src/ingest/extract.js"
import * as Record from "../../src/ingest/record.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "record-test-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const pcap = (name: string, bytes = "pcap"): string => {
  const file = path.join(scratch, name)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, bytes)
  return file
}

/** An anonymiser that records a failure as `failed.txt` in the capture's directory. */
const extraction = Extraction.layerWith({
  documentName: (capture) => Effect.succeed(`${capture}.flows.json`),
  anonymise: () => Effect.die("unused"),
  keptSeed: () => Effect.succeed(undefined),
  carrySeed: () => Effect.die("unused"),
  failureOf: (dir) =>
    Effect.sync(() => {
      const file = path.join(dir, "failed.txt")
      return fs.existsSync(file) ? fs.readFileSync(file, "utf8") : undefined
    }),
  recordFailure: () => Effect.die("unused"),
  sourceRefusal: () => Effect.die("unused")
})

const run = <A, E>(effect: Effect.Effect<A, E, FileSystem.FileSystem | Extraction.Service>) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.merge(extraction, NodeServices.layer))))

describe("Record.staleness", () => {
  const artifacts = path.join(scratch, "store")
  const source = { capture: "capture_s.pcap.gz", pcaps: [pcap("src/capture_s.pcap.gz", "one")] }

  const record = (
    over: Partial<{ extractor: string; size: number; sha256: string; pcaps: ReadonlyArray<string> }>
  ): void => {
    const dir = path.join(artifacts, source.capture)
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(
      path.join(dir, Record.INGEST),
      JSON.stringify({
        capture: source.capture,
        pcaps: (over.pcaps ?? source.pcaps).map((file) => {
          const stat = fs.statSync(file)
          return { path: file, size: over.size ?? stat.size, mtime: stat.mtime.toISOString(), sha256: over.sha256 ?? "" }
        }),
        extractor: over.extractor ?? "extractor-one",
        ingestedAt: new Date().toISOString()
      })
    )
  }

  it("calls a capture with no record never extracted", async () => {
    expect(await run(Record.staleness(artifacts, source, "extractor-one"))).toBe("never extracted")
  })

  it("agrees with an untouched pcap and refuses a changed extractor", async () => {
    record({})
    expect(await run(Record.staleness(artifacts, source, "extractor-one"))).toBeUndefined()
    expect(await run(Record.staleness(artifacts, source, "extractor-two"))).toBe("the extractor changed")
  })

  it("hashes the pcap where size or mtime moved, and trusts the bytes", async () => {
    // The record says a different size, so the bytes are read: they do not match
    // the hash it states either, and the capture is stale.
    record({ size: 999, sha256: "not-this-file" })
    expect(await run(Record.staleness(artifacts, source, "extractor-one"))).toBe("the pcap changed")
  })

  it("dates a set by every file of it, and by the set itself", async () => {
    const one = pcap("set/ring.pcap0", "one")
    const two = pcap("set/ring.pcap1", "two")
    const set = { capture: "ring.pcap0_set2", pcaps: [one, two] }
    const dir = path.join(artifacts, set.capture)
    fs.mkdirSync(dir, { recursive: true })
    const entry = (file: string, bytes: string) => ({
      path: file,
      size: fs.statSync(file).size,
      mtime: fs.statSync(file).mtime.toISOString(),
      sha256: crypto.createHash("sha256").update(bytes).digest("hex")
    })
    const write = (pcaps: ReadonlyArray<unknown>): void =>
      fs.writeFileSync(
        path.join(dir, Record.INGEST),
        JSON.stringify({ capture: set.capture, pcaps, extractor: "extractor-one", ingestedAt: new Date().toISOString() })
      )
    write([entry(one, "one"), entry(two, "two")])
    expect(await run(Record.staleness(artifacts, set, "extractor-one"))).toBeUndefined()
    // The second file's bytes moved: the whole set is stale.
    fs.writeFileSync(two, "two more")
    expect(await run(Record.staleness(artifacts, set, "extractor-one"))).toBe("the pcap changed")
    // The same name over a different list of files is a different set.
    write([entry(one, "one")])
    expect(await run(Record.staleness(artifacts, set, "extractor-one"))).toBe("the set of files changed")
  })
})

describe("Record.failureOf", () => {
  const artifacts = path.join(scratch, "failures")
  const leave = (capture: string, failed: string | undefined, ingest?: unknown): void => {
    const dir = path.join(artifacts, capture)
    fs.mkdirSync(dir, { recursive: true })
    if (failed !== undefined) fs.writeFileSync(path.join(dir, "failed.txt"), failed)
    if (ingest !== undefined) fs.writeFileSync(path.join(dir, Record.INGEST), JSON.stringify(ingest))
  }

  it("reads the failure the anonymiser recorded, with the size of every file the record holds", async () => {
    leave("capture_short.pcap.gz", "sipflow failed: x", {
      capture: "capture_short.pcap.gz",
      pcaps: [
        { path: "/p", size: 30, mtime: "2019-04-10T12:47:10.000Z", sha256: "0" },
        { path: "/q", size: 10, mtime: "2019-04-10T12:47:10.000Z", sha256: "0" }
      ],
      extractor: "e",
      ingestedAt: "2026-09-12T07:21:53.703Z"
    })
    expect(await run(Record.failureOf(artifacts, "capture_short.pcap.gz"))).toEqual({
      capture: "capture_short.pcap.gz",
      error: "sipflow failed: x",
      size: 40
    })
  })

  it("states no size where no ingest record was written", async () => {
    leave("capture_bare.pcap.gz", "the extraction died")
    expect(await run(Record.failureOf(artifacts, "capture_bare.pcap.gz"))).toEqual({
      capture: "capture_bare.pcap.gz",
      error: "the extraction died"
    })
  })

  it("is no failure where the extraction recorded none, or never ran", async () => {
    leave("capture_ok.pcap.gz", undefined)
    expect(await run(Record.failureOf(artifacts, "capture_ok.pcap.gz"))).toBeUndefined()
    expect(await run(Record.failureOf(artifacts, "capture_never.pcap.gz"))).toBeUndefined()
  })
})

describe("the raw document of an extraction", () => {
  it("is named so no walk for flows documents can take it", () => {
    expect(Extract.rawFlowsName("cap.pcap.gz").endsWith(".flows.json")).toBe(false)
  })
})
