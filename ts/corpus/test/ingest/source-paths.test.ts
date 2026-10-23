/**
 * A source added `--paths-from FILE`: a selection of thousands of pcaps is a
 * file of paths, never a shell glob a spawn is handed as one argument, and
 * the launched ingest job reads the same file.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { ingestArgs } from "../../src/jobs/job-start.js"
import * as Source from "../../src/ingest/source.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "paths-from-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

describe("Source.readPathList", () => {
  it("keeps a content-named copy of a list file, its entries absolute, which the job reads whatever the file becomes", async () => {
    const list = path.join(scratch, "lists", "picked.txt")
    fs.mkdirSync(path.dirname(list), { recursive: true })
    fs.writeFileSync(list, "one.pcap.gz\n/abs/two.pcapng\n")
    const keep = path.join(scratch, "jobs")
    const read = await Effect.runPromise(Source.readPathList(list, scratch, keep).pipe(Effect.provide(NodeServices.layer)))
    if (typeof read === "string") throw new Error(read)
    expect(read.paths).toEqual([path.join(scratch, "lists", "one.pcap.gz"), "/abs/two.pcapng"])
    expect(path.dirname(read.file)).toBe(keep)
    fs.writeFileSync(list, "changed.pcap\n")
    expect(Source.pathsFromText(fs.readFileSync(read.file, "utf8"))).toEqual(read.paths)
  })
})

describe("Source.pathsFromText", () => {
  it("reads one path per line, blank lines and # comments skipped, whitespace trimmed", () => {
    expect(Source.pathsFromText("a.pcap.gz\n\n  # a note\n b/c.pcapng \r\nd.pcap\n")).toEqual([
      "a.pcap.gz",
      "b/c.pcapng",
      "d.pcap"
    ])
  })
})

describe("the launched ingest job", () => {
  it("is handed the list file, not the paths it lists", () => {
    const argv = ingestArgs(5, {
      paths: ["/box/one.pcap.gz"],
      pathsFrom: "/box/list.txt",
      set: [],
      captures: Array.from({ length: 10_000 }, (_, at) => `capture_${at}.pcap.gz`),
      cut: false,
      concurrency: 8,
      memoryMax: "4G"
    })
    expect(argv).toContain("--paths-from")
    expect(argv[argv.indexOf("--paths-from") + 1]).toBe("/box/list.txt")
    expect(argv).toContain("/box/one.pcap.gz")
    expect(argv.join(" ").length).toBeLessThan(1000)
  })
})
