/**
 * The QUARANTINE tree: what a refusal deleted, written where the falsifier can
 * replay it.
 *
 * A refusal is a claim that no replayable case can be cut from this call, and
 * the only thing that can test the claim is a replay. In the case
 * generator's quarantine mode every refused case is assembled anyway into
 * `<out>/<capture>/refused/<id>/`, and this file is the contract between the
 * generator that writes it and the falsifier that reads it.
 *
 * The tree is NOT a case tree and never enters the cut store: a quarantined
 * document is evidence about a rule, not a test of this platform.
 */
import * as Schema from "effect/Schema"

/** Where a capture's quarantined documents go, beside its `cases/`. */
export const REFUSED_DIR = "refused"

/** The manifest naming every refused case of one capture, beside those documents. */
export const REFUSED_FILE = "refused.json"

/** One refused case, and whether the falsifier has a document to replay. */
export const RefusedCase = Schema.Struct({
  caseId: Schema.String,
  /** Every rule that refused it, in the order they were decided. */
  reasons: Schema.Array(Schema.String),
  /** Whether `<capture>/refused/<caseId>/<caseId>.v3.json` exists to replay. */
  built: Schema.Boolean,
  /**
   * Why there is no document to replay: assembly threw, or no lane can run the
   * one it assembled. Absent where one was written.
   */
  error: Schema.optionalKey(Schema.String)
})
export interface RefusedCase extends Schema.Schema.Type<typeof RefusedCase> {}

export const decodeRefused = Schema.decodeUnknownSync(Schema.Array(RefusedCase))

export const emitRefused = (entries: ReadonlyArray<RefusedCase>): string =>
  `${JSON.stringify(Schema.encodeUnknownSync(Schema.Array(RefusedCase))(entries), null, 2)}\n`

/**
 * What ONE quarantined case came to when it was replayed.
 *
 * All five are answers, and only the first is a defect: `falsified` means the
 * refusal refuses more than it must, `holds` means the failure IS the refusal's
 * evidence, `unbuildable` means the rule is unfalsifiable by replay and stands
 * on its own reading, `vacuous` means the document asserts NOTHING — a run of
 * it passes by doing nothing, so it can contradict no claim — and `unrun` means
 * the falsifier never got to it.
 */
export type Falsification = "falsified" | "holds" | "unbuildable" | "vacuous" | "unrun"

/** One refused case, replayed. Reported grouped by `reason`, never by capture. */
export interface Verdict {
  readonly capture: string
  readonly caseId: string
  readonly reasons: ReadonlyArray<string>
  readonly outcome: Falsification
  /** The cell's own last word, or why there was no cell. */
  readonly detail: string
}
