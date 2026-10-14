/**
 * The capture NAME a pcap takes, and which captures an invocation's inputs
 * come to. The name is the load-bearing claim: every record of a store is
 * keyed by it, so it has to come out byte for byte the same every time — a
 * rename would make the whole store's history unreachable.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Discover from "../../src/ingest/discover.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "discover-test-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const pcap = (name: string, bytes = "pcap"): string => {
  const file = path.join(scratch, name)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, bytes)
  return file
}

const run = <A>(effect: Effect.Effect<A, never, FileSystem.FileSystem>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

describe("Discover.captureOf", () => {
  it("drops a trailing .pcap and nothing else", () => {
    expect(Discover.captureOf("/pcaps/capture_a.pcap")).toBe("capture_a")
    expect(Discover.captureOf("/pcaps/capture_a.pcap.gz")).toBe("capture_a.pcap.gz")
    expect(Discover.captureOf("/pcaps/capture_a.pcapng")).toBe("capture_a.pcapng")
  })

  it("replaces every character the store key cannot carry", () => {
    expect(Discover.captureOf("/pcaps/one two (3).pcap.gz")).toBe("one_two__3_.pcap.gz")
  })
})

describe("Discover.captureOfSet", () => {
  it("names a one-file set as the file, and a longer one by its first file and its count", () => {
    expect(Discover.captureOfSet(["/pcaps/ring_00000.pcap"])).toBe("ring_00000")
    expect(Discover.captureOfSet(["/pcaps/ring_00000.pcap", "/pcaps/ring_00001.pcap"])).toBe("ring_00000_set2")
    expect(Discover.captureOfSet(["/pcaps/x.pcap.gz", "/pcaps/y.pcap.gz", "/pcaps/z.pcap.gz"])).toBe(
      "x.pcap.gz_set3"
    )
  })
})

describe("Discover.set", () => {
  it("orders the files as a ring names them, a directory contributing its pcaps", async () => {
    const dir = path.join(scratch, "ring")
    for (const n of [10, 9, 0, 1]) pcap(`ring/tap${n}.pcap`)
    pcap("ring/notes.txt")
    const late = pcap("late/tap11.pcap")
    const found = await run(Discover.set([late, dir, "gone"], scratch))
    expect(found.source?.capture).toBe("tap0_set5")
    expect(found.source?.pcaps.map((file) => path.basename(file))).toEqual([
      "tap0.pcap",
      "tap1.pcap",
      "tap9.pcap",
      "tap10.pcap",
      "tap11.pcap"
    ])
    expect(found.missing).toHaveLength(1)
    expect(found.missing[0]).toContain("gone")
  })

  it("takes the numbered files tcpdump -C leaves beside the first, under a directory", async () => {
    const dir = path.join(scratch, "tcpdump")
    for (const name of ["tap.pcap", "tap.pcap1", "tap.pcap2", "tap.pcap10", "tap.txt"]) pcap(`tcpdump/${name}`)
    const found = await run(Discover.set([dir], scratch))
    expect(found.source?.capture).toBe("tap_set4")
    expect(found.source?.pcaps.map((file) => path.basename(file))).toEqual([
      "tap.pcap",
      "tap.pcap1",
      "tap.pcap2",
      "tap.pcap10"
    ])
  })

  it("does not follow a link met inside a directory", async () => {
    const dir = path.join(scratch, "linked")
    pcap("linked/tap.pcap")
    fs.symlinkSync(pcap("elsewhere/tap.pcap1"), path.join(dir, "tap.pcap1"))
    const found = await run(Discover.set([dir], scratch))
    expect(found.source?.pcaps.map((file) => path.basename(file))).toEqual(["tap.pcap"])
  })

  it("is no source where nothing was named", async () => {
    expect((await run(Discover.set([], scratch))).source).toBeUndefined()
  })
})

describe("Discover.join", () => {
  it("adds the set beside the pcaps, unless a pcap already holds its name", () => {
    const one = { capture: "ring0", pcaps: ["/a/ring0.pcap"] }
    const each = { sources: [one], collisions: [], missing: ["--pcaps x names nothing at /x"] }
    const set = { source: { capture: "ring0_set2", pcaps: ["/b/ring0.pcap", "/b/ring1.pcap"] }, missing: [] }
    expect(Discover.join(each, set).sources.map((s) => s.capture)).toEqual(["ring0", "ring0_set2"])
    const same = { source: { capture: "ring0", pcaps: ["/b/ring0.pcap"] }, missing: ["--set y names nothing at /y"] }
    const joined = Discover.join(each, same)
    expect(joined.sources).toEqual([one])
    expect(joined.collisions).toHaveLength(1)
    expect(joined.collisions[0]).toContain("names the same capture as /a/ring0.pcap")
    expect(joined.missing).toHaveLength(2)
    expect(Discover.join(each, { source: undefined, missing: [] }).sources).toEqual([one])
  })
})

describe("Discover.isRingMember", () => {
  it("is a capture suffix, numbered or not", () => {
    for (const name of ["a.pcap", "a.pcap.gz", "a.pcapng", "a.pcapng.gz", "a.pcap7", "a.pcap7.gz"]) {
      expect(Discover.isRingMember(name), name).toBe(true)
    }
    for (const name of ["a.txt", "a.pcapx", "pcap", "a.pcap.bak"]) {
      expect(Discover.isRingMember(name), name).toBe(false)
    }
  })
})

describe("Discover.discover", () => {
  it("takes a directory recursively, a file as itself, and names what is missing", async () => {
    const dir = path.join(scratch, "tree")
    pcap("tree/a.pcap.gz")
    pcap("tree/deeper/b.pcapng")
    pcap("tree/notes.txt")
    const alone = pcap("alone/c.pcap")
    const found = await run(Discover.discover([dir, alone, "nowhere"], scratch))
    // Path order, not name order: the pcaps are worked in the order the host
    // holds them.
    expect(found.sources.map((source) => source.capture)).toEqual(["c", "a.pcap.gz", "b.pcapng"])
    expect(found.missing).toHaveLength(1)
    expect(found.missing[0]).toContain("nowhere")
  })

  it("keeps one capture per name and says which pcap lost", async () => {
    const first = pcap("dup1/same.pcap.gz")
    const second = pcap("dup2/same.pcap.gz")
    const found = await run(Discover.discover([first, second], scratch))
    expect(found.sources).toHaveLength(1)
    expect(found.collisions).toHaveLength(1)
    expect(found.collisions[0]).toContain("same.pcap.gz")
  })
})
