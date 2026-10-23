/**
 * What a queue row shows of a case document: the family, the number the first
 * call dialled, how that call ended, and the document's size in legs and
 * attempts. Read by duck-typing off whatever the row's document is, so a
 * document the strict contract refuses still summarises; every field is null
 * where the document lacks it, never guessed.
 */

export interface Summary {
  readonly family: string | null
  /** The `observed` number of the identity the first attempt calls. */
  readonly dialled: string | null
  /**
   * The final status of the first call's last attempt, as text; `no final`
   * where the last attempt got none; `refused by routing` / `abandoned` where
   * the call never dialled and says why.
   */
  readonly outcome: string | null
  readonly legs: number | null
  readonly attempts: number | null
}

export const EMPTY: Summary = { family: null, dialled: null, outcome: null, legs: null, attempts: null }

type Rec = Record<string, unknown>

const asRecord = (value: unknown): Rec | null =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Rec : null

const asString = (value: unknown): string | null => (typeof value === "string" ? value : null)

const asArray = (value: unknown): ReadonlyArray<unknown> | null => (Array.isArray(value) ? value : null)

const outcomeOf = (call: Rec, attempts: ReadonlyArray<unknown>): string => {
  const last = asRecord(attempts[attempts.length - 1])
  if (last !== null) {
    const status = asRecord(last["final"])?.["status"]
    return typeof status === "number" ? String(status) : "no final"
  }
  if (asRecord(call["abandoned"]) !== null) return "abandoned"
  if (asRecord(call["refused"]) !== null) return "refused by routing"
  return "no final"
}

const dialledOf = (document: Rec, attempts: ReadonlyArray<unknown>): string | null => {
  const name = asString(asRecord(asRecord(attempts[0])?.["callee"])?.["identity"])
  if (name === null) return null
  const identity = (asArray(document["identities"]) ?? [])
    .map(asRecord)
    .find((one) => one !== null && one["name"] === name)
  return identity === undefined || identity === null ? null : asString(identity["observed"])
}

/** The row-level facts of a document; every field null where it states none. */
export const summaryOf = (document: unknown): Summary => {
  const doc = asRecord(document)
  if (doc === null) return EMPTY
  const call = asRecord(asArray(doc["calls"])?.[0])
  const attempts = call === null ? null : asArray(call["attempts"])
  const legs = asArray(doc["legs"])
  return {
    family: asString(asRecord(doc["case"])?.["family"]),
    dialled: attempts === null ? null : dialledOf(doc, attempts),
    outcome: call === null || attempts === null ? null : outcomeOf(call, attempts),
    legs: legs === null ? null : legs.length,
    attempts: attempts === null ? null : attempts.length
  }
}
