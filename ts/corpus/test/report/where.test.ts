/**
 * Where one capture's artifacts are: a capture is known by any store that
 * names it, whatever its name's spelling, and its cases are counted unless
 * they are asked for.
 */
import { NodeServices } from "@effect/platform-node"
import { Cuts, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Where from "../../src/report/where.js"

/** A capture named with no pcap suffix, as an anonymised generation is. */
const C = "trace-one.anon2"
const root = fs.mkdtempSync(path.join(os.tmpdir(), "corpus-where-"))
const paths = { corpus: path.join(root, "corpus"), sessions: path.join(root, "sessions"), census: root, registry: root, repoRoot: root }
fs.mkdirSync(path.join(paths.corpus, C), { recursive: true })

afterAll(() => fs.rmSync(root, { recursive: true, force: true }))

const cuts = Layer.succeed(
  Cuts.Service,
  {
    standing: (capture: string) =>
      Effect.succeed(
        capture === C
          ? {
            prepId: 12,
            capture: C,
            flowsHash: null,
            broke: null,
            cutAt: "2026-10-12T00:00:00Z",
            cases: [1, 2, 3].map((n) => ({ caseId: `${C}-case${n}`, callKey: `k${n}@h`, caseHash: `hash${n}`, egressEndpoint: null }))
          }
          : null
      )
  } as unknown as Cuts.Interface
)

const runs = Layer.succeed(
  Runs.Service,
  { lanes: Effect.succeed([]), standing: () => Effect.succeed(null) } as unknown as Runs.Interface
)

const read = (capture: string, cases: boolean) =>
  Effect.runPromise(
    Where.read(paths, capture, { cases, cutCommand: (named) => `cut ${named}` }).pipe(
      Effect.provide(Layer.mergeAll(cuts, runs, NodeServices.layer))
    )
  )

describe("Where.read", () => {
  it("knows a capture the stores name, a name with no pcap suffix included", async () => {
    const where = await read(C, false)
    expect(where.known).toBe(true)
    expect(where.entries.find((entry) => entry.label === "cut")?.text).toContain("3 case(s)")
  })

  it("counts the cases unless they are asked for, and lists one line each when they are", async () => {
    expect(await read(C, false)).toMatchObject({ cases: 3 })
    expect((await read(C, false)).entries.filter((entry) => entry.label === "case")).toEqual([])
    expect((await read(C, true)).entries.filter((entry) => entry.label === "case")).toHaveLength(3)
  })

  it("says a capture no store names is unknown, and how a cut of it is recorded", async () => {
    const where = await read("trace-one", false)
    expect(where.known).toBe(false)
    expect(where.entries.find((entry) => entry.label === "cut")?.text).toBe("none — `cut trace-one` records one")
  })
})
