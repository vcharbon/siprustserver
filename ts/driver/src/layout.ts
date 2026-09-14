/**
 * The RUN DIR layout: where one campaign's cells and its index live.
 *
 * ```text
 * <run-dir>/
 *   campaign.json                 the aggregate index
 *   specs/<cell>.run-spec.json    what the driver asked the interpreter for
 *   <cell>/                       IS the interpreter's out_dir — it wipes it
 *   <cell>/error.txt              a cell that never produced a result
 *   <cell>/skipped.json           a cell skipped on a lane the document blocks
 * ```
 *
 * The run-spec sits OUTSIDE the cell directory on purpose: the interpreter owns
 * that directory and clears it before writing the bundle, so anything the driver
 * put there would be gone by the time a human went looking for it.
 */
import { Campaign, E2e } from "@sip/contracts"

/** The cell coordinate a campaign cell occupies in the matrix. */
export const cellIdOf = (cell: Campaign.CampaignCell): E2e.CellId => ({
  case: Campaign.cellCaseId(cell),
  shape: cell.kind,
  infra: cell.kind === "rust-test" ? cell.crate : cell.lane
})

/** The cell's directory name under the run dir. */
export const cellDir = (cell: Campaign.CampaignCell): string => E2e.cellDirName(cellIdOf(cell))

/** Where the driver keeps the specs it wrote, clear of the interpreter's out_dir. */
export const SPECS_DIR = "specs"

/** The aggregate index of one run. */
export const CAMPAIGN_INDEX = "campaign.json"

/** What a cell that never produced a result leaves behind. */
export const ERROR_FILE = "error.txt"

/** What a cell SKIPPED on a blocked lane leaves behind, in place of a bundle. */
export const SKIP_FILE = "skipped.json"

/** The run verdict the interpreter writes at the cell root. */
export const VERDICT_FILE = "verdict.json"

/** The lane-compiled run configuration the interpreter writes at the cell root. */
export const RUN_CONFIG_FILE = "run-config.json"

/** The post-run RFC audit the replay lane writes beside the verdict. */
export const RFC_FILE = "rfc.json"

/** The per-cell record an in-test reporter writes at the cell root. */
export const RESULT_FILE = "result.json"

/** The interpreter's per-leg recordings inside a replay cell. */
export const RECORDING_DIR = "recording"

/** The classified confrontation rows the driver writes at the cell root, post-run. */
export const CONFRONTATION_FILE = "confrontation.ndjson"

/** The per-cell classification summary the driver writes beside the rows. */
export const CLASSIFICATION_FILE = "classification.json"

/** The reclassifier's per-cell rule counts the driver writes at the cell root, post-run. */
export const RULE_HITS_FILE = "rule-hits.json"

/** The run-spec file name for one cell. */
export const runSpecFile = (cell: Campaign.CampaignCell): string =>
  `${cellDir(cell)}.run-spec.json`
