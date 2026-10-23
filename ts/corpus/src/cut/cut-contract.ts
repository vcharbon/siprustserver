/**
 * What a caller hands the cut, what the case generator says it did with each
 * capture, and what the cut phase makes of the pair.
 *
 * The first two live here rather than in the generator so a caller can read
 * them without importing an entry point that would run a cut on import. The
 * READING lives here because the phase produces it and the store consumes it,
 * and neither should have to import the other.
 */
import type { Model } from "@sip/store"
import * as Schema from "effect/Schema"

/** One capture's cut: where its enriched flows are, and where its cases go. */
export const CaptureWork = Schema.Struct({
  capture: Schema.String,
  /** Holds `<capture>.anon.flows.json`. */
  srcDir: Schema.String,
  /** Receives `cases/`, `excluded.json` and `names.json`; WIPED before the capture is cut. */
  outDir: Schema.String,
  /** The store's frozen case ids of the capture, with where each call starts: the names its calls keep. */
  frozen: Schema.optionalKey(Schema.Array(Schema.Struct({
    caseId: Schema.String,
    callKey: Schema.String,
    first: Schema.NullOr(Schema.Struct({ at_us: Schema.Number, src: Schema.String, dst: Schema.String }))
  })))
})
export interface CaptureWork extends Schema.Schema.Type<typeof CaptureWork> {}

/** The file beside a capture's cases stating, per case id, where its call starts and what key its id re-keys from. */
export const NAMES_FILE = "names.json"

/** One case id's line of {@link NAMES_FILE}. */
export interface NameLine {
  readonly start?: { readonly at_us: number; readonly src: string; readonly dst: string }
  readonly replaces?: string
}

/** A work list names directories the cut wipes, so it is decoded, never cast. */
export const decodeWorkList = Schema.decodeUnknownSync(Schema.Array(CaptureWork))

/**
 * What the cut did with one capture, one manifest line. A capture with NO line
 * was never reached; the line, never the scratch directory, is the answer.
 */
export const CaptureCut = Schema.Struct({
  capture: Schema.String,
  cut: Schema.Boolean,
  error: Schema.optionalKey(Schema.String)
})
export interface CaptureCut extends Schema.Schema.Type<typeof CaptureCut> {}

export const decodeCaptureCut = Schema.decodeUnknownSync(CaptureCut)

/** One case the cut produced, as the store takes it. */
export interface CutCase {
  readonly id: string
  /** The case directory the generator wrote, holding `<id>.v3.json` and `resources/`. */
  readonly dir: string
  /** The UAS actor's endpoint, where the cut named one other than the default. */
  readonly egressEndpoint?: string
}

/**
 * What the cut made of one capture, for the one prep that made it.
 *
 * A cut that REFUSED every case answers `cases: []` and no `broke` — an answer
 * rather than a failure. A cut that BROKE answers `broke` and no cases. The
 * store carries the two apart, and the queue reads the difference.
 */
export interface CutReading {
  readonly capture: string
  readonly prep: number
  readonly cutAt: string
  /** The enriched flows document every cell of this capture confronts. */
  readonly flows: string
  readonly provenance: Model.Provenance
  /** The cut CODE this reading came from, as a content hash. */
  readonly cutFingerprint: string
  /** Why there is nothing to replay, where the cut broke instead of refusing. */
  readonly broke?: string
  readonly cases: ReadonlyArray<CutCase>
  /** The cut's `excluded.json` for this capture; absent on disk where it refused nothing. */
  readonly excluded: string
  /** The cut's {@link NAMES_FILE} for this capture; absent on disk where it named nothing. */
  readonly names?: string
}
