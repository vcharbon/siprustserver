/**
 * The CALL KEY's syntax: a capture's own name for one call, as a replay
 * request cell and a cut's case row carry it.
 *
 * A key is a Call-ID, optionally followed by `#` and a qualifier that tells
 * apart the calls one Call-ID names in one capture. Which Call-ID names a case
 * and what qualifies it is the cut policy's rule; a reader of a key only ever
 * needs the Call-ID back, to match it against the Call-IDs a refusal names.
 */

/** The separator between the Call-ID and its qualifier. */
const QUALIFIER = "#"

/** A key of `callId`, qualified where the cut tells several calls of one Call-ID apart. */
export const make = (callId: string, qualifier?: string): string =>
  qualifier === undefined ? callId : `${callId}${QUALIFIER}${qualifier}`

/** The Call-ID a key names, its qualifier dropped. */
export const callIdOf = (key: string): string => {
  const at = key.indexOf(QUALIFIER)
  return at < 0 ? key : key.slice(0, at)
}
