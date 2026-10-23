/**
 * The generations of a capture name, end to end through the extraction with
 * stand-in tools: a capture the store holds without a kept seed lands under
 * its next generation and leaves the old one as it was; a capture whose kept
 * seed reproduces its anonymised bytes keeps its name; one whose bytes change
 * under its kept seed lands under the next generation — unless no citation
 * or real-clock run settles the name, which is extracted again in place.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Extraction, Tools } from "../../src/deployment/index.js"
import type { Source } from "../../src/ingest/discover.js"
import * as Extract from "../../src/ingest/extract.js"
import * as Generation from "../../src/ingest/generation.js"
import * as Record from "../../src/ingest/record.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "generation-test-"))
afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const tool = (name: string, body: string): string => {
  const file = path.join(scratch, "bin", name)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, body, { mode: 0o755 })
  return file
}

/** A stand-in extractor: the raw document is the pcap's bytes; an index is a marker file. */
const EXTRACTOR = tool("extractor", "#!/bin/sh\nif [ \"$1\" = index ]; then : > \"$2.idx\"; exit 0; fi\ncat \"$1\"\n")

/** What the stand-in anonymiser does: anonymise, or refuse every capture. */
let mode = "m1"

const seedOf = (artifacts: string, capture: string): string => path.join(artifacts, "seeds", capture)

/**
 * A stand-in anonymiser: the kept document is the raw one, the seed and the
 * mode — so the seed decides the bytes, and a changed mode is a changed
 * anonymiser output. A seed is minted where none is kept, before a refusal.
 */
const anonymiser = (broken = false) =>
  Extraction.layerWith({
    documentName: (capture) => Effect.succeed(`${capture}.flows.json`),
    anonymise: (request) =>
      Effect.gen(function*() {
        const seed = seedOf(request.artifacts, request.capture)
        if (!fs.existsSync(seed)) {
          fs.mkdirSync(path.dirname(seed), { recursive: true })
          fs.writeFileSync(seed, `${Math.random()}`)
        }
        if (broken || mode === "refuse") {
          return yield* new Extraction.AnonymiseFailed({ capture: request.capture, line: "anonymiser failed: survives" })
        }
        const doc = `${fs.readFileSync(request.raw, "utf8")}${fs.readFileSync(seed, "utf8")}${mode}`
        fs.writeFileSync(path.join(request.dir, `${request.capture}.flows.json`), doc)
        fs.rmSync(path.join(request.dir, "failed.txt"), { force: true })
      }),
    keptSeed: (artifacts, capture) =>
      Effect.sync(() => (fs.existsSync(seedOf(artifacts, capture)) ? seedOf(artifacts, capture) : undefined)),
    carrySeed: (artifacts, from, to) => Effect.sync(() => fs.copyFileSync(seedOf(artifacts, from), seedOf(artifacts, to))),
    failureOf: (dir) =>
      Effect.sync(() => {
        const file = path.join(dir, "failed.txt")
        return fs.existsSync(file) ? fs.readFileSync(file, "utf8") : undefined
      }),
    recordFailure: (dir, _capture, error) => Effect.sync(() => fs.writeFileSync(path.join(dir, "failed.txt"), error)),
    sourceRefusal: () => Effect.die("unused")
  })

const tools = (extractor = EXTRACTOR) =>
  Tools.layerWith({
    ready: () => Effect.succeed(extractor),
    generator: Effect.die("unused"),
    measure: Effect.die("unused"),
    compileMs: Effect.succeed(0),
    decides: Effect.die("unused")
  })

const artifacts = path.join(scratch, "store")

const extract = (
  source: Source,
  extractor: string,
  options: { readonly settled?: Extract.Settled; readonly tool?: string; readonly broken?: boolean } = {}
) =>
  Effect.runPromise(
    Extract.extract(artifacts, source, extractor, options.tool ?? EXTRACTOR, undefined, options.settled).pipe(
      Effect.provide(Layer.mergeAll(tools(options.tool), anonymiser(options.broken), NodeServices.layer))
    )
  )

const read = (capture: string) => Effect.runPromise(Record.read(artifacts, capture).pipe(Effect.provide(NodeServices.layer)))

const failure = (capture: string) =>
  Effect.runPromise(
    Record.failureOf(artifacts, capture).pipe(Effect.provide(Layer.merge(anonymiser(), NodeServices.layer)))
  )

const raw = path.join(scratch, "raw", "cap.anon.pcap")
fs.mkdirSync(path.dirname(raw), { recursive: true })
fs.writeFileSync(raw, "raw-bytes")
// The raw archive is read-only: nothing an extraction keeps is written there.
fs.chmodSync(path.dirname(raw), 0o555)
afterAll(() => fs.chmodSync(path.dirname(raw), 0o755))
const source: Source = { capture: "cap.anon", pcaps: [raw] }

const rw = (name: string, bytes: string): string => {
  const file = path.join(scratch, "raw-rw", name)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, bytes)
  return file
}

/** Every file of a directory with its bytes. */
const snapshot = (dir: string): Record<string, string> =>
  Object.fromEntries(fs.readdirSync(dir).sort().map((f) => [f, fs.readFileSync(path.join(dir, f), "utf8")]))

describe("Generation.nameOf", () => {
  it("counts on behind .anon, and appends .anon<n> to any other name", () => {
    expect(Generation.nameOf("x.anon", 1)).toBe("x.anon")
    expect(Generation.nameOf("x.anon", 2)).toBe("x.anon2")
    expect(Generation.nameOf("x", 3)).toBe("x.anon3")
  })
})

describe("Generation.current and Generation.after over a gap", () => {
  const run = <A>(effect: Effect.Effect<A, unknown, never>) => Effect.runPromise(effect)
  const holding = (store: string, names: ReadonlyArray<string>) => {
    for (const name of names) fs.mkdirSync(path.join(store, name), { recursive: true })
    return store
  }

  it("stands at the highest generation held, whatever generation below it is gone", async () => {
    const store = holding(path.join(scratch, "gap-store"), ["gap.anon3", "other.anon2"])
    const at = await run(Generation.current(store, "gap.anon").pipe(Effect.provide(NodeServices.layer)))
    expect(at).toEqual({ capture: "gap.anon3", generation: 3 })
    const plain = holding(path.join(scratch, "gap-store-plain"), ["gap", "gap.anon4"])
    const atPlain = await run(Generation.current(plain, "gap").pipe(Effect.provide(NodeServices.layer)))
    expect(atPlain).toEqual({ capture: "gap.anon4", generation: 4 })
  })

  it("counts on past the highest generation held", async () => {
    const store = holding(path.join(scratch, "gap-store-after"), ["gap.anon3"])
    const next = await run(Generation.after(store, "gap.anon", 1).pipe(Effect.provide(NodeServices.layer)))
    expect(next).toEqual({ capture: "gap.anon4", generation: 4 })
  })

  it("reads only the names of the capture's own generations", () => {
    expect(Generation.generationOf("gap.anon", "gap.anon")).toBe(1)
    expect(Generation.generationOf("gap.anon", "gap.anon12")).toBe(12)
    expect(Generation.generationOf("gap", "gap.anon2")).toBe(2)
    for (const name of ["gap.anon1", "gap.anon02", "gap.anon2b", "gap.anon2.anon3", "gapx.anon2", "gap"]) {
      expect(Generation.generationOf("gap.anon", name)).toBeUndefined()
    }
  })

  it("stands a source at the highest generation held", async () => {
    holding(artifacts, ["gapped.anon3"])
    const standing = await run(
      Extract.standing(artifacts, { capture: "gapped.anon", pcaps: [raw] }).pipe(Effect.provide(NodeServices.layer))
    )
    expect(standing).toEqual({ capture: "gapped.anon3", pcaps: [raw], first: "gapped.anon" })
  })
})

describe("Extract.extract generations", () => {
  it("lands a capture held without a kept seed under its next generation, the old one untouched", async () => {
    mode = "m1"
    const old = path.join(artifacts, "cap.anon")
    fs.mkdirSync(old, { recursive: true })
    fs.writeFileSync(path.join(old, "cap.anon.flows.json"), "old anonymised bytes")
    fs.writeFileSync(
      path.join(old, Record.INGEST),
      JSON.stringify({ capture: "cap.anon", pcaps: [], extractor: "x0", ingestedAt: "t" })
    )
    const before = snapshot(old)
    const done = await extract(source, "x1")
    expect(done.broke).toBeUndefined()
    expect(done.capture).toBe("cap.anon2")
    expect(done.landed).toContain("cap.anon → cap.anon2")
    expect(done.landed).toContain("keeps no seed")
    expect(snapshot(old)).toEqual(before)
    const record = await read("cap.anon2")
    expect(record?.generation).toBe(2)
    expect(record?.follows).toBe("cap.anon")
    expect(record?.seedFile).toBe(seedOf(artifacts, "cap.anon2"))
    expect(fs.existsSync(path.join(artifacts, "cap.anon2", "cap.anon2.flows.json.idx"))).toBe(true)
    expect(fs.readdirSync(path.dirname(raw))).toEqual(["cap.anon.pcap"])
  })

  it("keeps the name where the kept seed reproduces the anonymised bytes", async () => {
    const standing = await Effect.runPromise(Extract.standing(artifacts, source).pipe(Effect.provide(NodeServices.layer)))
    expect(standing.capture).toBe("cap.anon2")
    const anon = path.join(artifacts, "cap.anon2", "cap.anon2.flows.json")
    const bytes = fs.readFileSync(anon, "utf8")
    const done = await extract(source, "x2")
    expect(done.capture).toBe("cap.anon2")
    expect(done.landed).toContain("same anonymised bytes")
    expect(fs.readFileSync(anon, "utf8")).toBe(bytes)
    expect((await read("cap.anon2"))?.extractor).toBe("x2")
    expect(fs.existsSync(path.join(artifacts, "cap.anon3"))).toBe(false)
  })

  it("lands under the next generation where the bytes change under the kept seed", async () => {
    mode = "m2"
    const before = snapshot(path.join(artifacts, "cap.anon2"))
    const done = await extract(source, "x3")
    expect(done.capture).toBe("cap.anon3")
    expect(done.landed).toContain("changed under its kept seed")
    expect(snapshot(path.join(artifacts, "cap.anon2"))).toEqual(before)
    expect((await read("cap.anon3"))?.follows).toBe("cap.anon2")
    const seeds = ["cap.anon2", "cap.anon3"].map((c) => fs.readFileSync(seedOf(artifacts, c), "utf8"))
    expect(seeds[1]).toBe(seeds[0])
  })

  it("names a capture the store never held as the pcap does", async () => {
    const done = await extract({ capture: "fresh", pcaps: [rw("fresh.pcap", "other")] }, "x1")
    expect(done.capture).toBe("fresh")
    expect((await read("fresh"))?.generation).toBe(1)
  })
})

describe("Extract.extract after a refused extraction", () => {
  const refused: Source = { capture: "refused", pcaps: [rw("refused.pcap", "refused-bytes")] }

  it("extracts again under the same name with the seed the refused run kept", async () => {
    mode = "refuse"
    const first = await extract(refused, "x1")
    expect(first.broke).toContain("anonymiser failed")
    const seed = fs.readFileSync(seedOf(artifacts, "refused"), "utf8")
    mode = "m1"
    const done = await extract(refused, "x2")
    expect(done.broke).toBeUndefined()
    expect(done.capture).toBe("refused")
    expect(done.landed).toContain("the last extraction failed")
    expect(fs.existsSync(path.join(artifacts, "refused.anon2"))).toBe(false)
    expect(await failure("refused")).toBeUndefined()
    expect(fs.readFileSync(seedOf(artifacts, "refused"), "utf8")).toBe(seed)
    expect(fs.readFileSync(path.join(artifacts, "refused", "refused.flows.json"), "utf8")).toContain(seed)
  })

  it("extracts a capture whose failed run kept no seed under the same name, with a fresh one", async () => {
    const empty: Source = { capture: "empty", pcaps: [rw("empty.pcap", "")] }
    const broken = tool("noextract", "#!/bin/sh\necho not a capture >&2\nexit 1\n")
    const first = await extract(empty, "x1", { tool: broken })
    expect(first.broke).toBe("sipflow failed: not a capture")
    expect(fs.existsSync(seedOf(artifacts, "empty"))).toBe(false)
    const done = await extract(empty, "x2")
    expect(done.capture).toBe("empty")
    expect(done.broke).toBeUndefined()
    expect((await read("empty"))?.seedFile).toBe(seedOf(artifacts, "empty"))
  })
})

describe("Extract.extract of a name no citation or real-clock run pins", () => {
  const open: Source = { capture: "open", pcaps: [rw("open.pcap", "open-bytes")] }
  const unsettled: Extract.Settled = () => false

  it("extracts again in place where its bytes change under its kept seed", async () => {
    mode = "m1"
    await extract(open, "x1")
    mode = "m2"
    const done = await extract(open, "x2", { settled: unsettled })
    expect(done.capture).toBe("open")
    expect(done.landed).toContain("no cited id or real-lane run pins it")
    expect(fs.existsSync(path.join(artifacts, "open.anon2"))).toBe(false)
    expect(fs.readFileSync(path.join(artifacts, "open", "open.flows.json"), "utf8").endsWith("m2")).toBe(true)
    expect((await read("open"))?.generation).toBe(1)
  })

  it("extracts again in place, with a fresh seed, where it keeps none", async () => {
    fs.rmSync(seedOf(artifacts, "open"))
    const done = await extract(open, "x3", { settled: unsettled })
    expect(done.capture).toBe("open")
    expect(done.broke).toBeUndefined()
    expect(fs.existsSync(seedOf(artifacts, "open"))).toBe(true)
    expect(fs.existsSync(path.join(artifacts, "open.anon2"))).toBe(false)
  })

  it("mints the next generation once the name is settled", async () => {
    mode = "m3"
    const done = await extract(open, "x4", { settled: () => true })
    expect(done.capture).toBe("open.anon2")
  })
})

describe("Extract.extract of a name whose failure was recorded over anonymised bytes", () => {
  it("never extracts it again in place: a byte change under its kept seed is the next generation", async () => {
    // The sequence that leaves both: the staged files moved in, then the index failed.
    const indexed: Source = { capture: "indexed", pcaps: [rw("indexed.pcap", "indexed-bytes")] }
    mode = "m1"
    await extract(indexed, "x1")
    const noIndex = tool("noindex", "#!/bin/sh\nif [ \"$1\" = index ]; then echo no index >&2; exit 1; fi\ncat \"$1\"\n")
    const failed = await extract(indexed, "x2", { tool: noIndex })
    expect(failed.broke).toBe("sipflow index failed: no index")
    expect(await failure("indexed")).toBeDefined()
    expect(fs.existsSync(path.join(artifacts, "indexed", "indexed.flows.json"))).toBe(true)
    const before = snapshot(path.join(artifacts, "indexed"))
    mode = "m2"
    const done = await extract(indexed, "x3")
    expect(done.capture).toBe("indexed.anon2")
    expect(snapshot(path.join(artifacts, "indexed"))).toEqual(before)
  })
})

describe("Extract.extract failure under a kept seed", () => {
  it("leaves the artifacts it would have replaced as they were", async () => {
    const dir = path.join(artifacts, "cap.anon3")
    const before = snapshot(dir)
    const done = await extract(source, "x4", { broken: true })
    expect(done.broke).toContain("anonymiser failed")
    expect(done.capture).toBe("cap.anon3")
    expect(snapshot(dir)).toEqual(before)
  })
})
