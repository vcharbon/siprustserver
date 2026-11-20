/**
 * The post-run RFC audit's attributions as text lines, read off a cell's
 * `rfc.json`: each finding the document cancelled as relayed from another
 * party, with the statement that cancelled it — "relayed: <rule> originated by
 * <actor> at <step>; the reference relays it too" — and each finding charged
 * to a scripted party, as a source fault or as information. `rfc.json`
 * arrives untyped, so a field this reader cannot read contributes no line.
 * A source fault fails nothing: a cell's verdict word is the SUT's, and says
 * where a passing cell carries one, so its line never reads as a failure.
 */

/** How a cell ended, in a word: passed (a source fault beside it, where it carries one), or failed. */
export const verdictWord = (passed: boolean, sourceFault: boolean): string =>
  passed ? (sourceFault ? "passed, a source fault beside it as information" : "passed") : "failed"

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined

/** The findings of `rfc`, each as a record; none where it is not an audit. */
const findingsOf = (rfc: unknown): ReadonlyArray<Record<string, unknown>> => {
  const findings = record(rfc)?.["findings"]
  if (!Array.isArray(findings)) return []
  return findings.flatMap((value: unknown) => {
    const one = record(value)
    return one === undefined ? [] : [one]
  })
}

/** One line per relayed finding of `rfc`, in the audit's order. */
export const relayedLines = (rfc: unknown): ReadonlyArray<string> =>
  findingsOf(rfc).flatMap((finding) => {
    const rule = finding["rule"]
    const causedBy = finding["caused_by"]
    const label = finding["cancelled_by"]
    if (typeof rule !== "string" || typeof causedBy !== "string") return []
    return [`  rfc ${rule}, caused by ${causedBy}: ${typeof label === "string" ? label : "relayed"}`]
  })

/**
 * One line per binding finding of `rfc` charged to a scripted party, in the
 * audit's order: a source fault, one the document states (the reference
 * shows it) or one it could not judge. An advisory finding has no line.
 */
export const partyLines = (rfc: unknown): ReadonlyArray<string> =>
  findingsOf(rfc).flatMap((finding) => {
    const rule = finding["rule"]
    const actor = finding["actor"]
    if (typeof rule !== "string" || typeof actor !== "string" || finding["advisory"] === true) return []
    const label = finding["cancelled_by"]
    const unjudged = finding["unjudged"]
    const said = finding["source_fault"] === true
      ? "source fault — the replayed party repeats a captured message where, in this run, the RFC no longer " +
        "allows it; the document does not state it"
      : typeof label === "string"
      ? `information — ${label}`
      : typeof unjudged === "string"
      ? `information, unjudged — ${unjudged}`
      : undefined
    return said === undefined ? [] : [`  rfc ${rule} by scripted ${actor}: ${said}`]
  })
