/**
 * `Extraction` — how a capture's raw flows document becomes the one the
 * artifact store keeps, and what a capture whose extraction failed reads as.
 *
 * The extractor reads the pcaps into a raw flows document in a scratch file;
 * this deployment's anonymiser turns it into the kept document in the
 * capture's directory, and refuses a document in which a value it must replace
 * survives. The raw file goes with the extraction whatever happens.
 *
 * An anonymisation is REPRODUCIBLE under the seed the deployment keeps for a
 * capture name: the anonymiser uses the kept one where there is one and keeps
 * a fresh one where there is none. Where and how a seed is kept is the
 * deployment's; the ingest only asks whether one is kept, and carries it to
 * the next generation of a name whose bytes changed under it. Which name the
 * kept document has, and how a failed extraction is recorded beside it, are
 * the deployment's too.
 *
 * The default composes no anonymiser and refuses every extraction by name:
 * a store that kept a raw document by default is one a capture's private
 * values leak into.
 */
import type { ExclusionRecord } from "@sip/pipeline/exclusion-record"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"

/** The anonymisation failed; `line` is the anonymiser's own word on why. */
export class AnonymiseFailed extends Schema.TaggedError<AnonymiseFailed>()("Corpus.AnonymiseFailed", {
  capture: Schema.String,
  line: Schema.String
}) {
  override get message(): string {
    return `${this.capture}: ${this.line}`
  }
}

/** One anonymisation: the raw document, the directory the kept one lands in, the store and the capture name. */
export interface AnonymiseRequest {
  readonly raw: string
  readonly dir: string
  readonly artifacts: string
  readonly capture: string
}

/** What the ingest left of a capture it could not extract. */
export interface Failure {
  readonly capture: string
  /** The extraction's own line, verbatim. */
  readonly error: string
  /** The source files' byte count, where the ingest recorded it. */
  readonly size?: number
}

/** No anonymiser is composed; the message says how to state one. */
export class ExtractionUnstated extends Schema.TaggedError<ExtractionUnstated>()("Corpus.ExtractionUnstated", {}) {
  override get message(): string {
    return "no anonymiser is composed — substitute Extraction.layerWith to say how a raw flows document " +
      "becomes the one the store keeps"
  }
}

export interface Interface {
  /** The kept flows document's file name for `capture`, in its directory. */
  readonly documentName: (capture: string) => Effect.Effect<string, ExtractionUnstated>
  /**
   * `request.raw` anonymised into `request.dir`, under the seed kept for the
   * capture name (kept afresh where none is); a success replaces any failure
   * recorded there.
   */
  readonly anonymise: (request: AnonymiseRequest) => Effect.Effect<void, AnonymiseFailed | ExtractionUnstated>
  /** Where the seed of `capture` is kept in the store `artifacts`; `undefined` where none is. */
  readonly keptSeed: (artifacts: string, capture: string) => Effect.Effect<string | undefined, ExtractionUnstated>
  /** The seed kept for `from`, kept for `to` as well: a next generation anonymised under the same seed. */
  readonly carrySeed: (
    artifacts: string,
    from: string,
    to: string
  ) => Effect.Effect<void, AnonymiseFailed | ExtractionUnstated>
  /** The line a failed extraction left in the capture's directory `dir`; `undefined` where it left none. */
  readonly failureOf: (dir: string, capture: string) => Effect.Effect<string | undefined, ExtractionUnstated>
  /** A failed extraction of `capture`, recorded in `dir` as the anonymiser records its own refusals. */
  readonly recordFailure: (dir: string, capture: string, error: string) => Effect.Effect<void, ExtractionUnstated>
  /** The refusal a capture whose extraction failed is recorded under at the cut. */
  readonly sourceRefusal: (failure: Failure) => Effect.Effect<ExclusionRecord, ExtractionUnstated>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Extraction") {}

/** The default: no anonymiser, so no capture is extracted; each member refuses when a phase reaches for it. */
export const layer = Layer.sync(Service, () => {
  const unstated = Effect.fail(new ExtractionUnstated())
  return Service.of({
    documentName: () => unstated,
    anonymise: () => unstated,
    keptSeed: () => unstated,
    carrySeed: () => unstated,
    failureOf: () => unstated,
    recordFailure: () => unstated,
    sourceRefusal: () => unstated
  })
})

/** A deployment's own extraction. */
export const layerWith = (extraction: Interface): Layer.Layer<Service> =>
  Layer.succeed(Service, Service.of(extraction))
