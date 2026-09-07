/**
 * The RUN-SPEC the driver emits: one case, one lane, one output directory, and
 * the compiled §4.3 overlay.
 *
 * The `lane` block is deliberately opaque here. Which systems play the SUT, and
 * on what ports, is a DEPLOYMENT's vocabulary — the interpreter binary defines
 * it and `replay schema` publishes it — so this module states the envelope and
 * passes the block through verbatim. Modelling it would mean this package
 * pretending to know lanes it has never seen.
 *
 * `out_dir` is owned by the run: the interpreter WIPES it before writing the
 * bundle, so nothing the driver wants to keep may be written there beforehand.
 */
import { Bundle, Canonical, STRICT } from "@sip/contracts"
import * as Schema from "effect/Schema"

/**
 * One lane preset as the interpreter takes it: internally tagged on `kind`, and
 * otherwise whatever that lane declares.
 */
export interface LaneBlock {
  readonly kind: string
  readonly [field: string]: unknown
}

/** One replay run. */
export interface RunSpec {
  /** A v3 document, or a case directory holding `scenario.json`. */
  readonly case: string
  /** Where the run bundle is written. The interpreter clears it first. */
  readonly out_dir: string
  readonly lane: LaneBlock
  readonly run: Bundle.RunOverlay
}

const encodeOverlay = Schema.encodeUnknownSync(Bundle.RunOverlay, STRICT)

/** The spec as it goes on disk: canonical, so two identical runs diff to nothing. */
export const emitRunSpec = (spec: RunSpec): string =>
  Canonical.format({
    case: spec.case,
    out_dir: spec.out_dir,
    lane: spec.lane,
    run: encodeOverlay(spec.run)
  })
