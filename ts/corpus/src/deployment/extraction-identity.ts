/**
 * The IDENTITY extraction (`Extraction`): the kept flows document is the raw
 * one, byte for byte, for a deployment whose captures hold nothing to replace
 * — their names, numbers and addresses those the RFCs reserve for examples.
 * It keeps no seed: the same raw document always yields the same kept one.
 *
 * A failed extraction is recorded as its line in {@link FAILURE} beside the
 * document, and a capture whose extraction failed is refused at the cut under
 * the one token {@link SOURCE_REFUSED}, the line verbatim as its evidence.
 */
import type { ExclusionRecord } from "@sip/pipeline/exclusion-record"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as path from "node:path"
import * as Extraction from "./extraction.js"

/** The kept flows document of `capture`, in its directory. */
export const documentName = (capture: string): string => `${capture}.flows.json`

/** The file a failed extraction leaves its line in, in the capture's directory. */
export const FAILURE = "extraction-failure.txt"

/** The refusal token of a capture whose extraction failed. */
export const SOURCE_REFUSED = "source_unreadable"

/** The refusal a capture whose extraction failed is recorded under. */
export const sourceRefusalOf = (failure: Extraction.Failure): ExclusionRecord => ({
  caseId: `${failure.capture}-source`,
  reason: SOURCE_REFUSED,
  legs: [],
  callIds: [],
  sut: [],
  evidence: [{ detail: failure.error }],
  line: `${failure.capture}: the whole capture EXCLUDED ${SOURCE_REFUSED} — ${failure.error}`
})

export const layer = Layer.effect(
  Extraction.Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const failed = (capture: string) => (error: { readonly message: string }) =>
      new Extraction.AnonymiseFailed({ capture, line: error.message })
    return Extraction.Service.of({
      seeded: false,
      documentName: (capture) => Effect.succeed(documentName(capture)),
      anonymise: (request) =>
        Effect.gen(function*() {
          yield* fs.copyFile(request.raw, path.join(request.dir, documentName(request.capture)))
          yield* fs.remove(path.join(request.dir, FAILURE), { force: true })
        }).pipe(Effect.mapError(failed(request.capture))),
      keptSeed: () => Effect.succeed(undefined),
      carrySeed: () => Effect.void,
      failureOf: (dir) =>
        fs.readFileString(path.join(dir, FAILURE)).pipe(Effect.orElseSucceed(() => undefined)),
      recordFailure: (dir, _capture, error) => fs.writeFileString(path.join(dir, FAILURE), error).pipe(Effect.orDie),
      sourceRefusal: (failure) => Effect.succeed(sourceRefusalOf(failure))
    })
  })
)
