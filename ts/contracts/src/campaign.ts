/**
 * The **campaign input**: the document a driver is pointed at, listing the cells
 * one campaign runs.
 *
 * Driver-owned, not Rust-owned — the one contract here that no serde struct
 * mirrors — so its own keys are camelCase like the e2e records. The one nested
 * shape that IS Rust-owned, a cell's {@link RunOverlay}, keeps its snake_case
 * spelling: it is passed through to the `replay` bin verbatim.
 *
 * The two cell kinds are listed UNIFORMLY, in one array, because a campaign is
 * one matrix: a rust-test cell and a pivot-replay cell land in the same run dir
 * and fold into the same `campaign.json`. They are internally tagged on `kind`.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { RunOverlay } from "./bundle/overlay.js"
import { STRICT } from "./strict.js"

/** One cargo test the driver runs, and whose in-test reporter writes the results. */
export const RustTestCell = Schema.Struct({
  kind: Schema.Literal("rust-test"),
  crate: Schema.String,
  /** The test name as the crate's runner takes it — the driver never parses it. */
  name: Schema.String
})
export interface RustTestCell extends Schema.Schema.Type<typeof RustTestCell> {}

/** One pivot document replayed on one lane. */
export const PivotReplayCell = Schema.Struct({
  kind: Schema.Literal("pivot-replay"),
  /** A v3 document, or a case directory holding `scenario.json`. */
  case: Schema.String,
  /** The lane preset's own name — deployment vocabulary, so an open token. */
  lane: Schema.String,
  /**
   * The §4.3 overlay, passed to the run verbatim. The escape hatch for a case
   * whose routing no compiler states yet.
   */
  overlay: Schema.optionalKey(RunOverlay),
  /** Overrides the endpoint the lane's egress targets, where the default reading is wrong. */
  egressEndpoint: Schema.optionalKey(Schema.String),
  /**
   * The capture-side flows document the post-run confrontation reads its header
   * references from. Absent for an authored case — receptions then count as
   * unreferenced rather than comparing against nothing.
   */
  flows: Schema.optionalKey(Schema.String)
})
export interface PivotReplayCell extends Schema.Schema.Type<typeof PivotReplayCell> {}

/** One cell of the campaign, whichever kind it is. */
export const CampaignCell = Schema.Union([RustTestCell, PivotReplayCell])
export type CampaignCell = typeof CampaignCell.Type

/** What one campaign runs. */
export const CampaignInput = Schema.Struct({
  campaign: Schema.String,
  cells: Schema.Array(CampaignCell)
})
export interface CampaignInput extends Schema.Schema.Type<typeof CampaignInput> {}

export const decodeCampaignInput = Schema.decodeUnknownEffect(CampaignInput, STRICT)
export const decodeCampaignInputSync = Schema.decodeUnknownSync(CampaignInput, STRICT)

/** Parse a campaign document from its text. */
export const parseCampaignInput = (text: string) =>
  Effect.suspend(() => decodeCampaignInput(JSON.parse(text) as unknown))

/**
 * The case id a cell contributes to the run: the replayed document's own
 * directory or file name, and a rust test's name. It is the `case` half of the
 * cell id, and what names the cell's directory.
 */
export const cellCaseId = (cell: CampaignCell): string =>
  cell.kind === "rust-test" ? cell.name : basename(cell.case)

const basename = (file: string): string => {
  const trimmed = file.replace(/\/+$/, "")
  const cut = trimmed.slice(trimmed.lastIndexOf("/") + 1)
  return cut.endsWith(".json") ? cut.slice(0, -".json".length) : cut
}
