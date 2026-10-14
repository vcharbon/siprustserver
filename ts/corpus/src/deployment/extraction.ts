/**
 * `Extraction` — how a capture's raw flows document becomes the one the
 * artifact store keeps, and what a capture whose extraction failed reads as.
 *
 * The extractor reads the pcaps into a raw flows document in a scratch file;
 * this deployment's anonymiser turns it into the kept document in the
 * capture's directory under the seed the store keeps for that capture, and
 * refuses a document in which a value it must replace survives. The raw file
 * goes with the extraction whatever happens. Which name the kept document has
 * and which report the anonymiser leaves beside it are the deployment's too.
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

/** The seed one anonymisation runs under: the kept file, and whether it is minted afresh into it. */
export interface Seed {
  readonly file: string
  readonly fresh: boolean
}

/** One anonymisation: the raw document, the directory the kept one lands in, under which capture name and seed. */
export interface AnonymiseRequest {
  readonly raw: string
  readonly dir: string
  readonly capture: string
  readonly seed: Seed
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
   * The report the anonymiser leaves in a capture's directory: `{ capture,
   * error, cases }`, an empty `cases` with an `error` where it refused.
   */
  readonly reportName: Effect.Effect<string, ExtractionUnstated>
  readonly anonymise: (request: AnonymiseRequest) => Effect.Effect<void, AnonymiseFailed | ExtractionUnstated>
  /** The refusal a capture whose extraction failed is recorded under at the cut. */
  readonly sourceRefusal: (failure: Failure) => Effect.Effect<ExclusionRecord, ExtractionUnstated>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Extraction") {}

/** The default: no anonymiser, so no capture is extracted; each member refuses when a phase reaches for it. */
export const layer = Layer.sync(Service, () => {
  const unstated = Effect.fail(new ExtractionUnstated())
  return Service.of({
    documentName: () => unstated,
    reportName: unstated,
    anonymise: () => unstated,
    sourceRefusal: () => unstated
  })
})

/** A deployment's own extraction. */
export const layerWith = (extraction: Interface): Layer.Layer<Service> =>
  Layer.succeed(Service, Service.of(extraction))
