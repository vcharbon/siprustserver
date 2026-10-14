/**
 * The seam between the ingest and the cut: a capture with no kept document is
 * either a source the ingest refused — the deployment's source-tier record —
 * or a break of the cut, and the two never read as one; a capture that has
 * one is enriched by the extractor straight into the cut's scratch.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { enrich, enrichAll, flowsOf, workOf } from "../../src/cut/enrich.js"
import { Extraction, Tools } from "../../src/deployment/index.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "enrich-test-"))
const corpus = path.join(scratch, "corpus")
afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

/** A stand-in extractor: `--enrich <doc>` prints the document behind a marker. */
const EXTRACTOR = path.join(scratch, "extractor")
fs.writeFileSync(EXTRACTOR, "#!/bin/sh\nprintf 'enriched:'; cat \"$2\"\n", { mode: 0o755 })

const services = Layer.mergeAll(
  Extraction.layerWith({
    documentName: (capture) => Effect.succeed(`${capture}.flows.json`),
    anonymise: () => Effect.die("unused"),
    keptSeed: () => Effect.die("unused"),
    carrySeed: () => Effect.die("unused"),
    failureOf: (dir) =>
      Effect.sync(() => {
        const file = path.join(dir, "failed.txt")
        return fs.existsSync(file) ? fs.readFileSync(file, "utf8") : undefined
      }),
    recordFailure: () => Effect.die("unused"),
    sourceRefusal: (failure) =>
      Effect.succeed({
        caseId: "source",
        reason: "source-refused",
        legs: [],
        callIds: [],
        sut: [],
        line: `${failure.capture}: ${failure.error}`
      })
  }),
  Tools.layerWith({
    ready: () => Effect.succeed(EXTRACTOR),
    generator: Effect.die("unused"),
    measure: Effect.die("unused"),
    compileMs: Effect.succeed(0),
    decides: Effect.die("unused")
  }),
  NodeServices.layer
)

const captureDir = (capture: string, failed?: string, document?: string): void => {
  const dir = path.join(corpus, capture)
  fs.mkdirSync(dir, { recursive: true })
  if (failed !== undefined) fs.writeFileSync(path.join(dir, "failed.txt"), failed)
  if (document !== undefined) fs.writeFileSync(path.join(dir, `${capture}.flows.json`), document)
}

const run = <A, E>(effect: Effect.Effect<A, E, Layer.Success<typeof services>>) =>
  Effect.runPromise(effect.pipe(Effect.provide(services)))

describe("enrich", () => {
  it("refuses a source the ingest could not read, under the deployment's source tier", async () => {
    captureDir("refused", "the read failed")
    const why = await run(enrich(workOf(path.join(scratch, "work"), "refused"), corpus, EXTRACTOR))
    expect(why?.kind).toBe("refused")
    expect(why?.kind === "refused" && why.record.line).toBe("refused: the read failed")
  })

  it("refuses on the ingest's record even where an older document survives", async () => {
    captureDir("replaced", "the read failed", "{}")
    const why = await run(enrich(workOf(path.join(scratch, "work"), "replaced"), corpus, EXTRACTOR))
    expect(why?.kind).toBe("refused")
  })

  it("breaks, not refuses, where the ingest recorded no failure and left no document", async () => {
    captureDir("gone")
    const why = await run(enrich(workOf(path.join(scratch, "work"), "gone"), corpus, EXTRACTOR))
    expect(why?.kind === "broke" && why.why).toContain("no flows doc at")
  })

  it("enriches a kept document into the scratch the generator reads", async () => {
    captureDir("kept", undefined, "{\"legs\":[]}")
    const work = workOf(path.join(scratch, "work"), "kept")
    expect(await run(enrich(work, corpus, EXTRACTOR))).toBeUndefined()
    expect(fs.readFileSync(await run(flowsOf(work)), "utf8")).toBe("enriched:{\"legs\":[]}")
  })
})

describe("enrichAll", () => {
  it("breaks a capture whose scratch cannot be written alone, and lets the batch behind it finish", async () => {
    captureDir("blocked", undefined, "{}")
    captureDir("beside", undefined, "{}")
    const shut = path.join(scratch, "shut")
    fs.mkdirSync(shut, { recursive: true })
    fs.chmodSync(shut, 0o500)
    try {
      const why = await run(enrichAll([workOf(shut, "blocked"), workOf(path.join(scratch, "open"), "beside")], corpus, 2))
      expect(why.get("blocked")?.kind).toBe("broke")
      expect(why.has("beside")).toBe(false)
    } finally {
      fs.chmodSync(shut, 0o700)
    }
  })
})
