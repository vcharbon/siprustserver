/**
 * One capture's ENRICHED flows document, in the scratch the caller owns.
 *
 * The extractor's `--enrich` reads the kept document into the next schema.
 * Nothing here decides anything: it turns a corpus directory into the two
 * directories a generator work-list entry names, so the cut and every other
 * reader prepare their captures through one copy.
 *
 * A capture that cannot be enriched answers with WHY and leaves the captures
 * beside it alone — and the why is two different things: a source the ingest
 * could not read is a REFUSAL of the capture, stated by the deployment's
 * source tier; a flows document that should exist and does not is a BREAK of
 * the cut.
 */
import type { ExclusionRecord } from "@sip/pipeline/exclusion-record"
import { IntoFile } from "@sip/toolchain"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"
import { Extraction, Tools } from "../deployment/index.js"
import * as Record from "../ingest/record.js"
import type { CaptureWork } from "./cut-contract.js"

/** Why one capture has no enriched flows document. */
export type Unenriched =
  | { readonly kind: "refused"; readonly record: ExclusionRecord }
  | { readonly kind: "broke"; readonly why: string }

const broke = (why: string): Unenriched => ({ kind: "broke", why })

/**
 * The two directories one capture is prepared in, under the caller's scratch:
 * the generator reads the first and WIPES the second.
 */
export const workOf = (scratch: string, capture: string): CaptureWork => ({
  capture,
  srcDir: path.join(scratch, capture, "src"),
  outDir: path.join(scratch, capture, "out")
})

/** The enriched flows document of a capture being cut, wherever its scratch is. */
export const flowsOf = Effect.fn("Enrich.flowsOf")(function*(work: CaptureWork) {
  const extraction = yield* Extraction.Service
  return path.join(work.srcDir, yield* extraction.documentName(work.capture))
})

/** What an enrichment reaches: the platform, the deployment's extractor and anonymiser. */
export type Requirements =
  | FileSystem.FileSystem
  | ChildProcessSpawner.ChildProcessSpawner
  | Tools.Service
  | Extraction.Service

const messageOf = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/**
 * One capture's enriched flows document, written where the cut and the
 * confronter both read it, by the extractor at `extractor` (brought up to date
 * by the caller). Answers with why it could not be written, so a
 * capture that cannot be enriched leaves the cut alone.
 */
export const enrich = (
  work: CaptureWork,
  corpus: string,
  extractor: string
): Effect.Effect<Unenriched | undefined, never, Requirements> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const extraction = yield* Extraction.Service
    // The ingest's record is its latest word on the file and outranks any flows
    // document an earlier, readable version of the pcap left behind.
    const failure = yield* Record.failureOf(corpus, work.capture)
    if (failure !== undefined) return { kind: "refused", record: yield* extraction.sourceRefusal(failure) } as const
    const source = path.join(corpus, work.capture, yield* extraction.documentName(work.capture))
    if (!(yield* fs.exists(source))) return broke(`no flows doc at ${source}`)
    // Straight to the file: the document of a big capture is hundreds of
    // megabytes, which no parent can hold as one buffer.
    const why = yield* IntoFile.run(extractor, ["--enrich", source], yield* flowsOf(work))
    if (why !== undefined) return broke(`sipflow --enrich failed: ${why}`)
    return undefined
  }).pipe(
    Effect.catchCause((cause) =>
      Effect.succeed(broke(`enriching this capture failed: ${messageOf(Cause.squash(cause))}`))
    )
  )

/**
 * Every capture's enriched flows document, capture -> why it has none.
 * Concurrent: an enrichment is a short, small process, and thousands of them
 * serialized are minutes of pure fork overhead.
 */
export const enrichAll = Effect.fn("Enrich.enrichAll")(function*(
  work: ReadonlyArray<CaptureWork>,
  corpus: string,
  concurrency: number
) {
  const extractor = yield* Effect.flatMap(Tools.Service, (tools) => tools.ready("extractor"))
  const results = yield* Effect.forEach(work, (one) => enrich(one, corpus, extractor), { concurrency })
  const failures = new Map<string, Unenriched>()
  for (const [index, why] of results.entries()) {
    const one = work[index]
    if (one !== undefined && why !== undefined) failures.set(one.capture, why)
  }
  return failures as ReadonlyMap<string, Unenriched>
})
