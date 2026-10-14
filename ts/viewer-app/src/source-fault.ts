/**
 * The source fault a call carries: a scripted party's violation the document
 * does not state. Information that fails nothing, so it is never a main error:
 * it is a marker of its own beside whatever the row says — the SUT's main
 * error on a failing call, `passed` on a passing one.
 */
import type { TriageCallRow, TriageSourceFault } from "./api.ts"

/** The source fault of a case row; null where it carries none or the cut refused the call. */
export const sourceOf = (row: TriageCallRow): TriageSourceFault | null =>
  row.kind === "case" ? row.sourceFault ?? null : null

/** A source fault in one line: `source fault · <rule> · <citation>`. */
export const sourceFaultText = (source: Pick<TriageSourceFault, "rule" | "rfc">): string =>
  `source fault · ${source.rule}${source.rfc === null ? "" : ` · ${source.rfc}`}`
