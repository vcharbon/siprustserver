/**
 * A finding's text cut to its one-line difference. An audit detail reads
 * `<address>: <what happened> (callId …) … — <the rule's prose> (RFC …)`:
 * the short form keeps what happened, without the leading address, the
 * Call-ID parenthesis and the rule's prose, which the full text keeps.
 */

/** A socket or endpoint as the audit prefixes a detail with it: `127.0.0.1:5060`, `[::1]:5060#ep0`. */
const ADDRESS_PREFIX = /^(?:\[[^\]]+\]|[\w.-]+):\d+(?:#\w+)?:\s+/

const CALL_ID = /\s*\(callId [^)]*\)/g

/** The detail's first clause: no leading address, no Call-ID, nothing past the first ` — `. */
export const shortDifference = (detail: string): string => {
  const unaddressed = detail.replace(ADDRESS_PREFIX, "")
  const dash = unaddressed.indexOf(" — ")
  const clause = dash < 0 ? unaddressed : unaddressed.slice(0, dash)
  return clause.replace(CALL_ID, "").trim()
}

/** The step a statement names, `… at s54 by uas1` → `s54`. */
export const stepNamed = (statement: string | undefined): string | undefined =>
  statement === undefined ? undefined : /\bat (s\d+)\b/.exec(statement)?.[1]
