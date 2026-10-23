/**
 * Reading an `excluded.json` — the generator's own refusal to cut a case, kept
 * on disk beside the cases it did write.
 *
 * A refusal is EVIDENCE, not an error. The directory holding one has no
 * scenario document, and the exclusion is what it has to show: the reason token
 * the category is greppable by, the calls it was charged on, the evidence down
 * to the datagram, and the one-line finding in the generator's own words.
 *
 * The file's shape belongs to the generator, so this reader is tolerant the way
 * the flows reader is: it takes the fields a reviewer must see, and a record it
 * cannot read becomes a warning rather than a silence.
 */

/** One datagram, leg or group an exclusion rests on. */
export interface ExclusionEvidence {
  /** The census rule, where the exclusion came from a census hit. */
  readonly rule: string | undefined
  readonly callId: string | undefined
  /** The charged socket, `ip:port`, as the census names it. */
  readonly emitter: string | undefined
  /** Coordinates into the flows document the finding was made on. */
  readonly group: number | undefined
  readonly leg: number | undefined
  readonly msg: number | undefined
  /** What was seen, in one clause. */
  readonly detail: string
}

/** One case the generator refused to write, with everything that decided it. */
export interface ExclusionRecord {
  readonly caseId: string
  /** The reason token: one per rule, so a category is greppable on its own. */
  readonly reason: string
  readonly callIds: readonly string[]
  /** The refused case's legs: every vantage the cut gave it. */
  readonly legs: readonly number[]
  /** The addresses the cut decided the SUT at, `ip` or `ip:port`; empty where the record states none. */
  readonly sut: readonly string[]
  readonly groups: readonly number[]
  readonly evidence: readonly ExclusionEvidence[]
  /** The generator's one-line finding, verbatim. */
  readonly line: string
}

export type ExclusionRead =
  | {
    readonly ok: true
    readonly records: readonly ExclusionRecord[]
    readonly warnings: readonly string[]
  }
  | { readonly ok: false; readonly reason: string }

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined

const str = (value: unknown): string | undefined =>
  typeof value === "string" ? value : undefined

const num = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined

const strings = (value: unknown): readonly string[] =>
  Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : []

const numbers = (value: unknown): readonly number[] =>
  Array.isArray(value)
    ? value.filter((entry): entry is number => typeof entry === "number" && Number.isFinite(entry))
    : []

const readEvidence = (value: unknown): ExclusionEvidence | undefined => {
  const entry = record(value)
  if (entry === undefined) return undefined
  return {
    rule: str(entry["rule"]),
    callId: str(entry["callId"]),
    emitter: str(entry["emitter"]),
    group: num(entry["group"]),
    leg: num(entry["leg"]),
    msg: num(entry["msg"]),
    detail: str(entry["detail"]) ?? ""
  }
}

/**
 * `excluded.json` as a list of refusals. The file is an ARRAY — one entry per
 * refused case and rule — and anything else is refused by shape rather than
 * coerced into one.
 */
export const parseExclusions = (value: unknown): ExclusionRead => {
  if (!Array.isArray(value)) {
    return { ok: false, reason: "an exclusion file is a JSON array of refusals" }
  }
  const warnings: string[] = []
  const records: ExclusionRecord[] = []
  value.forEach((entry, index) => {
    const raw = record(entry)
    const caseId = str(raw?.["caseId"])
    const reason = str(raw?.["reason"])
    if (raw === undefined || caseId === undefined || reason === undefined) {
      warnings.push(`entry ${index} names no case and no reason; it is not drawn`)
      return
    }
    records.push({
      caseId,
      reason,
      callIds: strings(raw["callIds"]),
      legs: numbers(raw["legs"]),
      sut: strings(raw["sut"]),
      groups: numbers(raw["groups"]),
      evidence: (Array.isArray(raw["evidence"]) ? raw["evidence"] : []).flatMap((item) => {
        const read = readEvidence(item)
        return read === undefined ? [] : [read]
      }),
      line: str(raw["line"]) ?? ""
    })
  })
  if (records.length === 0 && warnings.length === 0) {
    warnings.push("the file holds no refusal")
  }
  return { ok: true, records, warnings }
}

/** The reason tokens a listing badges an excluded entry with, deduplicated. */
export const exclusionReasons = (records: readonly ExclusionRecord[]): readonly string[] =>
  [...new Set(records.map((entry) => entry.reason))]

/** Every call the refusals name, deduplicated, in the order they appear. */
export const exclusionCallIds = (records: readonly ExclusionRecord[]): readonly string[] =>
  [...new Set(records.flatMap((entry) => entry.callIds))]
