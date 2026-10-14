/**
 * The source fault a call's main error stands beside: the main error names
 * the SUT's failure, and the scripted party's violation the document does not
 * state is a marker of its own next to it, so neither hides the other.
 */
import type { TriageMainError, TriageSourceFault } from "./api.ts"

/** The source fault beside `error`; null where it carries none or nothing was measured. */
export const sourceBeside = (error: TriageMainError | null): TriageSourceFault | null => error?.source ?? null

/** A source fault in one line: `source fault · <rule> · <citation>`. */
export const sourceFaultText = (source: Pick<TriageSourceFault, "rule" | "rfc">): string =>
  `source fault · ${source.rule}${source.rfc === null ? "" : ` · ${source.rfc}`}`
