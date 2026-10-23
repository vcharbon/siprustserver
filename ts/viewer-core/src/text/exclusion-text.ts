/**
 * A refusal as text, where an export prints a verdict: the reason token on its
 * own line, then the generator's line and every charged datagram under it.
 *
 * It is the exclusion pane flattened, in the generator's own words: the token
 * a category is greppable by first, so a run with no cell still states in one
 * line why.
 */
import type { ExclusionEvidence, ExclusionRecord } from "../exclusion/parse.ts"

/** Where one datagram sits: its coordinates, then the call it is on. */
const where = (entry: ExclusionEvidence): string =>
  [
    entry.rule === undefined ? undefined : entry.rule,
    entry.emitter === undefined ? undefined : `by ${entry.emitter}`,
    entry.group === undefined ? undefined : `group ${entry.group}`,
    entry.leg === undefined ? undefined : `leg ${entry.leg}`,
    entry.msg === undefined ? undefined : `msg ${entry.msg}`,
    entry.callId === undefined ? undefined : `on '${entry.callId}'`
  ].filter((part) => part !== undefined).join(" ")

/** One record: `excluded: <reason>`, its line, its evidence, each indented under it. */
const recordLines = (record: ExclusionRecord): readonly string[] => [
  `excluded: ${record.reason}`,
  ...(record.line === "" ? [] : [`  ${record.line}`]),
  ...record.evidence.map((entry) => {
    const at = where(entry)
    return `  evidence: ${at === "" ? entry.detail : `${at} — ${entry.detail}`}`
  })
]

/** Every record, in file order, one block each. */
export const exclusionsToText = (records: readonly ExclusionRecord[]): string =>
  records.flatMap(recordLines).join("\n")
