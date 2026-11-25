/**
 * Adoption of a named capture the artifact store lacks: a file directly under
 * a pcap root, named with its `.pcap` or without, resolves to its capture name
 * and is pending ingest; a held name stays; a name no root holds stays as
 * named for the store's refusal; two files for one name refuse; a dry run
 * ingests nothing. An ingest that stands failed refuses, naming the capture,
 * and a new generation's name replaces the one it renamed.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Roots } from "../../src/deployment/index.js"
import * as Adopt from "../../src/ingest/adopt.js"
import type { Source as Found } from "../../src/ingest/discover.js"
import type { Paths } from "../../src/paths.js"
import type { Selection } from "../../src/universe.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "adopt-test-"))
afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const paths: Paths = {
  corpus: path.join(scratch, "artifacts"),
  sessions: path.join(scratch, "sessions"),
  census: path.join(scratch, "census"),
  registry: path.join(scratch, "registry.json"),
  repoRoot: scratch
}
const pcaps = path.join(scratch, "pcaps")
const other = path.join(scratch, "other")
for (const dir of [paths.corpus, pcaps, other]) fs.mkdirSync(dir, { recursive: true })
fs.mkdirSync(path.join(paths.corpus, "held"))
fs.writeFileSync(path.join(pcaps, "taken.anon.pcap"), "pcap")
fs.writeFileSync(path.join(pcaps, "ring.pcap.gz"), "pcap")
fs.writeFileSync(path.join(pcaps, "twice.pcap"), "pcap")
fs.writeFileSync(path.join(other, "twice.pcap"), "another pcap")
fs.mkdirSync(path.join(pcaps, "sub"))
fs.writeFileSync(path.join(pcaps, "sub", "deep.pcap"), "pcap")

const layer = Layer.merge(
  Roots.layerWith({
    paths: () => Effect.succeed(paths),
    pcapRoots: Effect.succeed([pcaps, other]),
    uploadsRoot: Effect.succeed(false),
    locks: Effect.die("unused"),
    bench: Effect.die("unused")
  }),
  NodeServices.layer
)

const resolve = (names: ReadonlyArray<string>) => Effect.runPromise(Adopt.resolve(paths, names).pipe(Effect.provide(layer)))

const named = (positional: ReadonlyArray<string>): Selection => ({
  universe: "named",
  positional,
  onlyNew: false,
  maxSpanSeconds: undefined,
  greenOnLane: undefined,
  sampleSize: undefined,
  limit: undefined,
  cases: []
})

describe("Adopt.resolve", () => {
  it("takes a root's file by its capture name or its file name, pending ingest under the capture name", async () => {
    const read = await resolve(["taken.anon", "taken.anon.pcap", "ring.pcap.gz"])
    expect(read).toEqual({
      _tag: "resolved",
      positional: ["taken.anon", "taken.anon", "ring.pcap.gz"],
      pending: [
        { capture: "taken.anon", pcaps: [path.join(pcaps, "taken.anon.pcap")] },
        { capture: "ring.pcap.gz", pcaps: [path.join(pcaps, "ring.pcap.gz")] }
      ],
      renamed: [{ name: "taken.anon.pcap", capture: "taken.anon", file: path.join(pcaps, "taken.anon.pcap") }]
    })
  })

  it("keeps a held name and a name no root holds directly as named, pending nothing", async () => {
    expect(await resolve(["held", "nowhere", "deep", "sub/deep.pcap", "../pcaps/taken.anon.pcap"])).toEqual({
      _tag: "resolved",
      positional: ["held", "nowhere", "deep", "sub/deep.pcap", "../pcaps/taken.anon.pcap"],
      pending: [],
      renamed: []
    })
  })

  it("refuses a name two roots hold different files for, naming both", async () => {
    const read = await resolve(["twice"])
    expect(read._tag).toBe("refused")
    if (read._tag !== "refused") return
    expect(read.why).toContain(path.join(pcaps, "twice.pcap"))
    expect(read.why).toContain(path.join(other, "twice.pcap"))
  })

  it("counts a root reached through a link once", async () => {
    const linked = path.join(scratch, "linked")
    fs.symlinkSync(pcaps, linked)
    const read = await Effect.runPromise(
      Adopt.resolve(paths, ["taken.anon"]).pipe(
        Effect.provide(Layer.merge(
          Roots.layerWith({
            paths: () => Effect.succeed(paths),
            pcapRoots: Effect.succeed([pcaps, linked]),
            uploadsRoot: Effect.succeed(false),
            locks: Effect.die("unused"),
            bench: Effect.die("unused")
          }),
          NodeServices.layer
        ))
      )
    )
    expect(read._tag === "resolved" && read.pending.map((one) => one.capture)).toEqual(["taken.anon"])
  })
})

describe("Adopt.fromRootsBy", () => {
  /** An ingest that lands every source it is handed, recording them. */
  const recording = () => {
    const handed: Array<ReadonlyArray<Found>> = []
    const ingest: Adopt.Ingester<never, never> = (sources) =>
      Effect.sync(() => {
        handed.push(sources)
        return {
          sources,
          outcomes: new Map(sources.map((one) => [one.capture, { _tag: "ingested", landed: "new" } as const])),
          renamed: new Map()
        }
      })
    return { handed, ingest }
  }
  const adopt = (selection: Selection, dryRun: boolean, ingest: Adopt.Ingester<never, never>) =>
    Effect.runPromise(Adopt.fromRootsBy(paths, selection, { tag: "t", concurrency: 1, dryRun }, ingest).pipe(Effect.provide(layer)))

  it("ingests the pending captures once each and answers the list under their capture names", async () => {
    const { handed, ingest } = recording()
    const answer = await adopt(named(["taken.anon.pcap", "held", "taken.anon"]), false, ingest)
    expect(answer).toEqual({ _tag: "named", positional: ["taken.anon", "held"], ingested: ["taken.anon"] })
    expect(handed).toEqual([[{ capture: "taken.anon", pcaps: [path.join(pcaps, "taken.anon.pcap")] }]])
  })

  it("in a dry run answers what it would ingest and ingests nothing", async () => {
    const { handed, ingest } = recording()
    const answer = await adopt(named(["taken.anon.pcap", "held"]), true, ingest)
    expect(answer).toEqual({ _tag: "named", positional: ["taken.anon", "held"], ingested: ["taken.anon"] })
    expect(handed).toEqual([])
  })

  it("ingests nothing for a list the store holds, nor for a derived universe", async () => {
    const { handed, ingest } = recording()
    expect(await adopt(named(["held", "nowhere"]), false, ingest)).toEqual({
      _tag: "named",
      positional: ["held", "nowhere"],
      ingested: []
    })
    expect(await adopt({ ...named([]), universe: "all" }, false, ingest)).toEqual({ _tag: "named", positional: [], ingested: [] })
    expect(handed).toEqual([])
  })
})

describe("Adopt.settled", () => {
  const source = { capture: "taken.anon", pcaps: [path.join(pcaps, "taken.anon.pcap")] }

  it("answers the ingested captures, a new generation's name in place of the one it renamed", () => {
    const { answer, lines } = Adopt.settled(["held", "taken.anon"], {
      sources: [{ ...source, capture: "taken.anon-2", first: "taken.anon" }],
      outcomes: new Map([["taken.anon-2", { _tag: "ingested", landed: "taken.anon → taken.anon-2: new" }]]),
      renamed: new Map([["taken.anon", "taken.anon-2"]])
    })
    expect(answer).toEqual({ _tag: "named", positional: ["held", "taken.anon-2"], ingested: ["taken.anon-2"] })
    expect(lines).toEqual(["   +  taken.anon-2  ingested — taken.anon → taken.anon-2: new"])
  })

  it("refuses where an ingest stands failed, now or before, naming the capture", () => {
    for (
      const outcome of [
        { _tag: "failed", why: "unreadable pcap", kind: "run", landed: "" } as const,
        { _tag: "current", failed: "unreadable pcap" } as const
      ]
    ) {
      const { answer, lines } = Adopt.settled(["taken.anon"], {
        sources: [source],
        outcomes: new Map([["taken.anon", outcome]]),
        renamed: new Map()
      })
      expect(answer._tag).toBe("refused")
      expect(answer._tag === "refused" && answer.why).toContain("taken.anon did not ingest")
      expect(lines[0]).toContain("unreadable pcap")
    }
  })
})
