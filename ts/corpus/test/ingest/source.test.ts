/**
 * A source added by path: the roots rule — a path is read only under the
 * store's `_uploads` or a root the deployment names, followed through links —
 * the ONE ingest judgement a campaign and the source list share, and the
 * per-capture line a cut prints.
 */
import { NodeServices } from "@effect/platform-node"
import { type Model, Population, Sources } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { readingLine } from "../../src/cut/cut.js"
import type { CutReading } from "../../src/cut/cut-contract.js"
import { Roots } from "../../src/deployment/index.js"
import type { Source as Found } from "../../src/ingest/discover.js"
import * as Record from "../../src/ingest/record.js"
import * as Source from "../../src/ingest/source.js"
import type { Paths } from "../../src/paths.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "source-test-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const paths: Paths = {
  corpus: path.join(scratch, "artifacts"),
  sessions: path.join(scratch, "sessions"),
  census: path.join(scratch, "census"),
  registry: path.join(scratch, "registry.json"),
  repoRoot: scratch
}

/** The deployment's pcap roots: `named`, absolute. */
const rootsAt = (named: ReadonlyArray<string>) =>
  Roots.layerWith({
    paths: () => Effect.succeed(paths),
    pcapRoots: Effect.succeed(named),
    locks: Effect.die("unused"),
    bench: Effect.die("unused")
  })

const refusal = (inputs: ReadonlyArray<string>, cwd: string, named: ReadonlyArray<string>) =>
  Effect.runPromise(
    Source.refusal(inputs, cwd, paths).pipe(Effect.provide(Layer.merge(rootsAt(named), NodeServices.layer)))
  )

describe("Source.refusal", () => {
  const pcaps = path.join(scratch, "pcaps")
  fs.mkdirSync(pcaps, { recursive: true })

  it("takes a path under a root the deployment names, or under the store's _uploads", async () => {
    expect(await refusal([path.join(pcaps, "a.pcap")], scratch, [pcaps])).toBeNull()
    expect(await refusal(["pcaps/sub/b.pcap.gz"], scratch, [pcaps])).toBeNull()
    expect(await refusal([path.join(paths.corpus, Source.UPLOADS, "drop.pcap")], scratch, [pcaps])).toBeNull()
  })

  it("refuses a path outside every root, naming the roots", async () => {
    const refused = await refusal([path.join(scratch, "elsewhere", "a.pcap"), "pcaps/ok.pcap"], scratch, [pcaps])
    expect(refused).toContain("elsewhere/a.pcap")
    expect(refused).not.toContain("ok.pcap")
    expect(refused).toContain("outside the roots")
    expect(refused).toContain(pcaps)
    expect(refused).toContain(path.join(paths.corpus, Source.UPLOADS))
    // A relative escape out of a root is outside it.
    expect(await refusal(["pcaps/../elsewhere/a.pcap"], scratch, [pcaps])).not.toBeNull()
  })

  it("follows a link into a root: a root may be a link to the host's copy", async () => {
    const real = path.join(scratch, "real-pcaps")
    fs.mkdirSync(real, { recursive: true })
    fs.writeFileSync(path.join(real, "a.pcap"), "pcap")
    const link = path.join(scratch, "linked-pcaps")
    fs.symlinkSync(real, link)
    // Named through the link: under the root as stated.
    expect(await refusal([path.join(link, "a.pcap")], scratch, [link])).toBeNull()
    // Named by its real path: under the root once the root is followed.
    expect(await refusal([path.join(real, "a.pcap")], scratch, [link])).toBeNull()
  })

  it("names the store's uploads first, then the deployment's roots", async () => {
    const roots = await Effect.runPromise(
      Source.roots(paths).pipe(Effect.provide(rootsAt(["/srv/pcaps", "/srv/more"])))
    )
    expect(roots).toEqual([path.join(paths.corpus, Source.UPLOADS), "/srv/pcaps", "/srv/more"])
  })
})

const EXTRACTOR = "extractor-now"

/** A population holding these rows, read as the source list reads it. */
const populationOf = (rows: ReadonlyArray<Model.PopulationMember>) =>
  Layer.succeed(
    Population.Service,
    Population.Service.of({ list: Effect.succeed(rows) } as unknown as Population.Interface)
  )

const member = (capture: string, rest: Partial<Model.PopulationMember>): Model.PopulationMember => ({
  capture,
  sourcePath: null,
  addedAt: "2026-09-20T00:00:00.000Z",
  ingestedAt: "2026-09-20T00:00:00.000Z",
  extractorHash: EXTRACTOR,
  ingestFailed: null,
  ...rest
})

const staleness = (sources: ReadonlyArray<Found>, artifacts: string, rows: ReadonlyArray<Model.PopulationMember>) =>
  Effect.runPromise(
    Source.staleness(sources, artifacts, EXTRACTOR).pipe(
      Effect.provide(Layer.merge(populationOf(rows), NodeServices.layer))
    )
  )

describe("Source.staleness — a campaign's `needs ingest` and the source list's `ingest` read one judgement", () => {
  const artifacts = path.join(scratch, "artifacts")
  const pcap = (name: string): string => {
    const file = path.join(scratch, "pcaps", name)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, "pcap")
    return file
  }
  /** A current ingest record for the capture, as the extraction leaves one. */
  const record = (source: Found): void => {
    const dir = path.join(artifacts, source.capture)
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(
      path.join(dir, Record.INGEST),
      JSON.stringify({
        capture: source.capture,
        pcaps: source.pcaps.map((file) => {
          const stat = fs.statSync(file)
          return { path: file, size: stat.size, mtime: stat.mtime.toISOString(), sha256: "" }
        }),
        extractor: EXTRACTOR,
        ingestedAt: "2026-09-20T00:00:00.000Z"
      })
    )
  }

  it("calls a capture stale for a campaign exactly where the source list calls it stale", async () => {
    const sources: Array<Found> = ["current", "old-extractor", "no-hash", "never", "failed", "absent"]
      .map((name) => ({ capture: `${name}.pcap.gz`, pcaps: [pcap(`${name}.pcap.gz`)] }))
    for (const source of sources) record(source)
    const rows = [
      member("current.pcap.gz", {}),
      member("old-extractor.pcap.gz", { extractorHash: "extractor-old" }),
      // Ingested before the extractor was recorded on the row: stale on both sides until re-ingested.
      member("no-hash.pcap.gz", { extractorHash: null }),
      member("never.pcap.gz", { ingestedAt: null }),
      member("failed.pcap.gz", { ingestFailed: "sipflow failed: unreadable" })
    ]
    const why = await staleness(sources, artifacts, rows)
    for (const row of rows) {
      const listed = Sources.ingestState(row, EXTRACTOR)
      expect([row.capture, why.has(row.capture)]).toEqual([row.capture, listed === "stale"])
    }
    expect(why.get("old-extractor.pcap.gz")).toBe("the population's extractor is not this tree's")
    expect(why.get("no-hash.pcap.gz")).toBe("the population's extractor is not this tree's")
    expect(why.get("never.pcap.gz")).toBe("the population has no ingest of it")
    // A capture the population does not hold is stale, whatever the artifact store keeps of it.
    expect(why.get("absent.pcap.gz")).toBe("not in the population")
    // A failure the caller's own extractor recorded stands: neither side asks for another extraction.
    expect(why.has("failed.pcap.gz")).toBe(false)
  })

  it("asks the artifact store only once the row is current: a pcap whose bytes moved is the campaign's to see", async () => {
    const source = { capture: "moved.pcap.gz", pcaps: [pcap("moved.pcap.gz")] }
    record(source)
    fs.writeFileSync(source.pcaps[0]!, "pcap, but more of it")
    const row = member("moved.pcap.gz", {})
    const why = await staleness([source], artifacts, [row])
    expect(Sources.ingestState(row, EXTRACTOR)).toBe("current")
    expect(why.get("moved.pcap.gz")).toBe("the pcap changed")
  })
})

const reading = (rest: Partial<CutReading>): CutReading => ({
  capture: "capture_a.pcap.gz",
  prep: 12,
  cutAt: "2026-09-20T00:00:00.000Z",
  flows: "flows.json",
  excluded: "excluded.json",
  cutFingerprint: "cut-code",
  provenance: [],
  cases: [],
  ...rest
})

describe("the per-capture cut line", () => {
  it("says cut with the case count, broke with the reason, or refused-all — each with the prep", () => {
    expect(readingLine(reading({ cases: [{ id: "c1", dir: "/w/c1" }, { id: "c2", dir: "/w/c2" }] })))
      .toBe("[cut] prep12 capture_a.pcap.gz: cut (2 case(s))")
    expect(readingLine(reading({ broke: "[prep12] the cut failed: boom" })))
      .toBe("[cut] prep12 capture_a.pcap.gz: broke — [prep12] the cut failed: boom")
    expect(readingLine(reading({}))).toBe("[cut] prep12 capture_a.pcap.gz: refused-all")
  })
})
