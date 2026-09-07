/**
 * The triage registry (`testkit/triage/registry.json`): curated knowledge in
 * git, one verdict per triage session, saying which diverging captures stay out
 * of the queue and why.
 *
 * ONE pretty-printed JSON object keyed by the corpus capture BASENAME — the
 * pcap's own filename, so a record survives re-cuts and re-extraction unchanged.
 * Cut case ids are only semi-stable and a `cut` verdict indicts the cut itself,
 * so a record narrows itself to specific cases via `cases` instead; absence
 * means whole-capture scope.
 *
 * The verdict alone discriminates finality: `expected` / `never-fix` are FINAL,
 * `cut` / `replay` / `bug` are PENDING and must carry a `ticket` path. There is
 * no separate `kind` field to drift out of sync with the verdict.
 *
 * EVIDENCE is provenance, not build input: a run-bundle path (typically under a
 * gitignored work dir) plus a step token into the confrontation output. It may
 * dangle on another machine; the registry stays authoritative without it.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"

/** Where a triage session proved the verdict: run bundle + step, human note. */
export const Evidence = Schema.Struct({
  /** Repo-relative run-bundle path. */
  bundle: Schema.String,
  /** Step token in that bundle's confrontation output. */
  step: Schema.optionalKey(Schema.String),
  note: Schema.optionalKey(Schema.String)
})
export interface Evidence extends Schema.Schema.Type<typeof Evidence> {}

const common = {
  /** ISO date of the triage session that wrote the record. */
  decided_on: Schema.String,
  evidence: Evidence,
  /** Narrows the record to specific cut case ids; absent = whole capture. */
  cases: Schema.optionalKey(Schema.Array(Schema.String))
}

/** Capture permanently excluded from the diverge queue. */
export const FinalRecord = Schema.Struct({
  verdict: Schema.Literals(["expected", "never-fix"]),
  ...common,
  /** Rule id in the hint file that generalized this verdict. */
  hint: Schema.optionalKey(Schema.String)
})
export interface FinalRecord extends Schema.Schema.Type<typeof FinalRecord> {}

/** Capture suppressed while the linked ticket is open; re-queues on close. */
export const PendingRecord = Schema.Struct({
  verdict: Schema.Literals(["cut", "replay", "bug"]),
  /** Repo-relative ticket path. */
  ticket: Schema.String,
  ...common
})
export interface PendingRecord extends Schema.Schema.Type<typeof PendingRecord> {}

export const RegistryRecord = Schema.Union([FinalRecord, PendingRecord])
export type RegistryRecord = FinalRecord | PendingRecord

/** `registry.json`: capture basename → record, keys sorted. */
export const Registry = Schema.Record(Schema.String, RegistryRecord)
export type Registry = { readonly [capture: string]: RegistryRecord }

export const isFinal = (record: RegistryRecord): record is FinalRecord =>
  record.verdict === "expected" || record.verdict === "never-fix"

/**
 * A pending record's ticket, resolved to a state. `closed` and `missing` both
 * re-queue the capture — a verdict whose ticket vanished is unproven, so the
 * re-queue on `missing` is deliberate, not a degraded mode. `wontfix` keeps the
 * capture suppressed and proposes converting the record to a final `never-fix`;
 * the conversion itself is human-confirmed, never automatic.
 */
export const TicketState = Schema.Literals(["open", "closed", "wontfix", "missing"])
export type TicketState = typeof TicketState.Type

/** What a sweep does with a diverging capture, given its registry record. */
export const Disposition = Schema.Literals(["excluded", "suppressed", "requeue", "untriaged"])
export type Disposition = typeof Disposition.Type

/**
 * The ticket's `Status:` line, resolved to a state. Only the line's leading
 * token decides, case-insensitively: real ticket files append dates and notes
 * after it (`Status: resolved (2026-08-25)`), and equality matching would read
 * every one of those as open. `undefined` text means the ticket file itself is
 * gone.
 */
export const ticketStateOfText = (text: string | undefined): TicketState => {
  if (text === undefined) return "missing"
  const line = text.split("\n").find((candidate) => candidate.trimStart().toLowerCase().startsWith("status:"))
  const token = line?.split(":", 2)[1]?.trim().split(/[\s(]/, 1)[0]?.toLowerCase() ?? ""
  if (token === "resolved" || token === "done" || token === "closed") return "closed"
  if (token === "wontfix") return "wontfix"
  return "open"
}

/**
 * The checker: no record means the capture is takeable, a final record excludes
 * it permanently, and a pending one rides its ticket — suppressed while the
 * ticket is open (or wontfix, awaiting its conversion), re-queued once it
 * closes or goes missing. Pure on purpose: the caller reads the registry and
 * the ticket file, so the same ruling serves the viewer, the sweep, and any
 * script without this package touching a filesystem.
 */
export const disposition = (
  record: RegistryRecord | undefined,
  ticket: TicketState | undefined
): Disposition => {
  if (record === undefined) return "untriaged"
  if (isFinal(record)) return "excluded"
  return ticket === "closed" || ticket === "missing" ? "requeue" : "suppressed"
}

export class TriageRegistryError extends Schema.TaggedError<TriageRegistryError>()("Triage.RegistryError", {
  operation: Schema.String,
  reason: Schema.String
}) {}

export const decodeRegistry = Schema.decodeUnknownEffect(Registry, STRICT)
export const decodeRegistrySync = Schema.decodeUnknownSync(Registry, STRICT)

export const parseRegistry = (text: string) => Effect.suspend(() => decodeRegistry(JSON.parse(text) as unknown))
