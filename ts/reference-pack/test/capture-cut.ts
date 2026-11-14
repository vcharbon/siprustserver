/**
 * A committed capture ingested and cut as the corpus does it: extracted by
 * `sipflow` with the identity extraction (`@sip/corpus` `ExtractionIdentity`)
 * into an artifacts directory, projecting the headers the base's default leg
 * rules read, then cut by the base's case generator (`@sip/corpus`
 * `cut/base-generator.ts`) under those rules into an output directory.
 */
import { NodeServices } from "@effect/platform-node"
import { ExtractionIdentity, Tools } from "@sip/corpus"
import { BASE } from "@sip/corpus/cut/generator"
import * as LegRules from "@sip/corpus/cut/leg-rules"
import * as Extract from "@sip/corpus/ingest/extract"
import * as Record from "@sip/corpus/ingest/record"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"

/**
 * The extractor `sipflow` alone, projecting the headers the base's default
 * leg rules read: the ingest reaches for no other tool.
 */
const toolsOf = (sipflow: string) =>
  Layer.effect(
    Tools.Service,
    Effect.map(LegRules.legRulesOf(LegRules.DEFAULT_RULES), (rules) =>
      Tools.Service.of({
        ready: () => Effect.succeed(sipflow),
        generator: Effect.succeed(BASE),
        measure: Effect.die("the ingest measures nothing"),
        compileMs: Effect.succeed(0),
        decides: Effect.die("the ingest decides nothing"),
        reading: [],
        emitHeaders: LegRules.headersOf(rules)
      }))
  )

/** The capture file `file` ingested as `capture` into `artifacts` with the identity extraction by `sipflow`. */
export const ingest = (sipflow: string, artifacts: string, capture: string, file: string) =>
  Effect.runPromise(
    Extract.phase([{ capture, pcaps: [file] }], artifacts, "sipflow", 1).pipe(
      Effect.provide(Layer.merge(toolsOf(sipflow), ExtractionIdentity.layer).pipe(Layer.provideMerge(NodeServices.layer)))
    )
  )

/** The kept flows document of `capture` in `artifacts`. */
export const documentOf = (artifacts: string, capture: string): string =>
  path.join(Record.captureDir(artifacts, capture), ExtractionIdentity.documentName(capture))

/** How the system under test derives the Call-ID of a leg it dials (`--call-id-derivation`). */
export type Derivation = "none" | "counter-prefix"

/**
 * The base generator run over the directory of `capture` in `artifacts` into
 * `out`, joining legs by `derivation`: its manifest's lines. Its work list
 * and manifest sit beside `out`.
 */
export const cut = (artifacts: string, capture: string, out: string, derivation: Derivation): ReadonlyArray<unknown> => {
  const list = path.join(out, "..", `${capture}.work-list.json`)
  const manifest = path.join(out, "..", `${capture}.manifest.jsonl`)
  fs.mkdirSync(out, { recursive: true })
  fs.rmSync(manifest, { force: true })
  fs.writeFileSync(list, JSON.stringify([{ capture, srcDir: Record.captureDir(artifacts, capture), outDir: out }]))
  const [command, ...args] = BASE.argv
  execFileSync(command!, [...args, "--work-list", list, "--manifest", manifest, "--call-id-derivation", derivation], {
    cwd: BASE.cwd,
    stdio: "pipe"
  })
  return fs.readFileSync(manifest, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as unknown)
}
