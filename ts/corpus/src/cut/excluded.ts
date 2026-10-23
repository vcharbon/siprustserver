/**
 * The cut's `excluded.json`, read as store refusals: the on-disk record the
 * generator writes beside the cases (`ExclusionRecord`), reduced to what a
 * queue row states of a refused call — its case id, the reasons, the Call-IDs
 * it rests on, the record's own line and the call key that freezes its id.
 * The file holds one record per refusing verdict; a call several verdicts
 * refused is read as one refusal under every reason, in file order.
 *
 * A file that is missing or unreadable is no refusals, never an error: a cut
 * that refused nothing writes none.
 */
import { type Model, Refusals } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { stemOf } from "../paths.js"

/** What the generator records beside the cases when it refuses some of them. */
export const EXCLUDED = "excluded.json"

/** Where the cut leaves what it refused of `capture` under the session tree. */
export const excludedFile = (sessions: string, capture: string): string =>
  path.join(sessions, stemOf(capture), capture, EXCLUDED)

const asRecord = (value: unknown): Record<string, unknown> | null =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null

const strings = (value: unknown): ReadonlyArray<string> =>
  Array.isArray(value) ? value.filter((one): one is string => typeof one === "string") : []

/** One record as a store refusal; null where it names no case or no reason. */
export const refusalOf = (record: unknown): Model.Refusal | null => {
  const entry = asRecord(record)
  if (entry === null || typeof entry["caseId"] !== "string" || typeof entry["reason"] !== "string") return null
  const evidence = Array.isArray(entry["evidence"])
    ? entry["evidence"].flatMap((one) => {
      const detail = asRecord(one)?.["detail"]
      return typeof detail === "string" ? [detail] : []
    })
    : []
  return {
    caseId: entry["caseId"],
    reasons: [entry["reason"]],
    callIds: strings(entry["callIds"]),
    detail: typeof entry["line"] === "string" ? entry["line"] : evidence.join("; "),
    ...(typeof entry["callKey"] === "string" ? { callKey: entry["callKey"] } : {})
  }
}

/** Every refused call the records hold, one refusal per call, in their order. */
export const refusalsOf = (records: unknown): ReadonlyArray<Model.Refusal> => {
  if (!Array.isArray(records)) return []
  return Refusals.merge(records.flatMap((record) => {
    const refusal = refusalOf(record)
    return refusal === null ? [] : [refusal]
  }))
}

/** Every refused call the file holds, one refusal per call, in file order. */
export const readRefusals = Effect.fn("Excluded.readRefusals")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const records = yield* fs.readFileString(file).pipe(
    Effect.flatMap((text) => Effect.try(() => JSON.parse(text) as unknown)),
    Effect.orElseSucceed(() => undefined)
  )
  return refusalsOf(records)
})

/** The distinct reasons the file refused calls under, in file order; none where it holds no record. */
export const readReasons = (file: string) =>
  Effect.map(readRefusals(file), (refusals) => [...new Set(refusals.flatMap((refusal) => refusal.reasons))])
