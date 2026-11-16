/**
 * COUNTING ONE CELL against the post-run rule families: which delta rule
 * named which difference, whether the bless restated the cell and on whose
 * strength, and what the bless audit found where nothing needed it.
 *
 * The count is taken off the reading the reclassifier already made, so a cell
 * is parsed once; what leaves here is numbers under open tokens, in the
 * `CellHits` form the driver writes beside the cell.
 */
import type { CellHits, Confrontation } from "@sip/contracts"
import type { Reclassifier } from "@sip/pipeline"
import { auditFindings, blessing } from "./premise.js"
import type * as RfcInherit from "./rfc-inherit.js"

/** The bless premise's own token: what a restated cell is counted under. */
export const BLESS_PREMISE = "bless-premise"

/** The rule a record with no name aggregates under: a hole, not a wide rule. */
export const UNNAMED = "(unnamed)"

/** One cell's counts, keyed by family, rule and bucket, in first-seen order. */
class CellTally {
  private readonly at = new Map<string, CellHits.Hit>()

  add(family: string, rule: string, bucket: string): void {
    const key = `${family} ${rule} ${bucket}`
    const held = this.at.get(key)
    if (held === undefined) this.at.set(key, { family, rule, bucket, hits: 1 })
    else this.at.set(key, { ...held, hits: held.hits + 1 })
  }

  rows(): CellHits.Hits {
    return [...this.at.values()]
  }
}

/**
 * Every post-run rule count one cell carries. `outcome.passed` is the gate the
 * reclassifier read, and decides whether the audit is put to the cell.
 *
 * - `tolerance <rule> <class>`: each record under its rule, and `shape` beside
 *   it for a shape record, the only kind the bless premise reads.
 * - `bless bless-premise fired`, and `tolerance <rule> blessed` for each rule
 *   the restated cell stood on.
 * - `bless <finding> fired`: each audit finding on a passing cell.
 * - `rfc <rule> relayed` and `rfc <rule> source-fault`: the findings the
 *   document attributes to a relayed peer, and a scripted party's own.
 */
export const cellHits = (
  outcome: Reclassifier.RunOutcome,
  records: ReadonlyArray<Confrontation.ConfrontationRecord>,
  rfc?: RfcInherit.Ruling
): CellHits.Hits => {
  const tally = new CellTally()
  for (const record of records) {
    const rule = record.rule === "" ? UNNAMED : record.rule
    tally.add("tolerance", rule, record.class)
    if (record.kind === "shape") tally.add("tolerance", rule, "shape")
  }
  const blessed = blessing(outcome.verdict.failures ?? [], records, outcome.verdict.abandoned)
  if (blessed !== undefined) {
    tally.add("bless", BLESS_PREMISE, "fired")
    for (const rule of blessed) tally.add("tolerance", rule, "blessed")
  }
  if (outcome.passed) {
    for (const finding of auditFindings(outcome, records, rfc)) {
      tally.add("bless", finding.finding, "fired")
    }
  }
  for (const finding of rfc?.relayed ?? []) tally.add("rfc", finding.rule, "relayed")
  for (const finding of rfc?.source ?? []) tally.add("rfc", finding.rule, "source-fault")
  return tally.rows()
}
