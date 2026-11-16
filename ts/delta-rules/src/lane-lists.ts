/**
 * A deployment's per-lane lists and the classification they decide: which
 * rules a lane ACCEPTS forever and which it holds as a KNOWN BUG pending a
 * ticket. A list carries a class and a ticket and nothing else; the ruling
 * behind an entry is the rule's own `why`. The file syntax, the `extends`
 * chain (nearest wins) and the loader's refusals:
 *
 * - a list naming a rule the deployment does not define refuses to load;
 * - a `known_bug` entry without a ticket, or an `accepted` entry with one,
 *   refuses;
 * - one rule in both classes of one file refuses;
 * - an `extends` cycle refuses.
 *
 * Classification: the first rule that matches decides the NAME; the lane's
 * lists decide the CLASS — `accepted` / `known-bug` when listed, `unlisted`
 * when named but not listed, `unknown` when no rule names it.
 */
import type { Confrontation } from "@sip/contracts"
import type { Classifier, Probe } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type { PlatformError } from "effect/PlatformError"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { type DeltaRule, matchRule } from "./rule.js"

/** A list of the chain is not there. */
export class LaneListMissing extends Schema.TaggedError<LaneListMissing>()("DeltaRules.LaneListMissing", {
  file: Schema.String
}) {}

/** A list of the chain is there and the platform will not hand it over. */
export class LaneListUnreadable extends Schema.TaggedError<LaneListUnreadable>()("DeltaRules.LaneListUnreadable", {
  file: Schema.String,
  cause: Schema.Defect()
}) {}

/** A list is not JSON, not the schema, or breaks one of the loader's refusals. */
export class LaneListCorrupt extends Schema.TaggedError<LaneListCorrupt>()("DeltaRules.LaneListCorrupt", {
  file: Schema.String,
  reason: Schema.String
}) {}

/** Every way a lane list fails to load. */
export type LaneListError = LaneListMissing | LaneListUnreadable | LaneListCorrupt

const Entry = Schema.Struct({
  rule: Schema.String,
  ticket: Schema.optionalKey(Schema.String)
})

/** One listed rule, and the ticket a `known_bug` entry waits on. */
export interface Entry extends Schema.Schema.Type<typeof Entry> {}

const LaneFile = Schema.Struct({
  lane: Schema.String,
  extends: Schema.optionalKey(Schema.String),
  accepted: Schema.Array(Entry),
  known_bug: Schema.Array(Entry)
})
const decodeLaneFile = Schema.decodeUnknownEffect(LaneFile, { onExcessProperty: "error" })

/** One lane's resolved policy: rule id → class + entry, nearest lane first. */
export interface LanePolicy {
  readonly lane: string
  readonly accepted: ReadonlyMap<string, Entry>
  readonly knownBug: ReadonlyMap<string, Entry>
}

/** Where a lane's lists are read from, and the rule ids they may name. */
export interface ListSource {
  /** The directory holding `<name>.json` for every list of a chain. */
  readonly dir: string
  /** A directory whose `<name>.deltas.json` stands in for the list `name`, where it holds one. */
  readonly beside?: string | undefined
  /** Every rule id the deployment defines: a list naming another refuses. */
  readonly known: ReadonlySet<string>
}

/** The open known-bug entries, sorted by rule id — the ratchet's read. */
export const openBugs = (policy: LanePolicy): ReadonlyArray<Entry> =>
  [...policy.knownBug.values()].sort((a, b) => a.rule.localeCompare(b.rule))

/** The file of the list `name`: `<beside>/<name>.deltas.json` where `beside` holds one, else `<dir>/<name>.json`. */
const listFile = Effect.fn("DeltaRules.laneListFile")(function* (name: string, dir: string, beside: string | undefined) {
  if (beside !== undefined) {
    const own = path.join(beside, `${name}.deltas.json`)
    const fs = yield* FileSystem.FileSystem
    if (yield* fs.exists(own).pipe(Effect.orElseSucceed(() => false))) return own
  }
  return path.join(dir, `${name}.json`)
})

const readFailure = (file: string) => (error: PlatformError): LaneListMissing | LaneListUnreadable =>
  error.reason._tag === "NotFound"
    ? new LaneListMissing({ file })
    : new LaneListUnreadable({ file, cause: error })

/** The list at `file`, decoded. */
const readList = Effect.fn("DeltaRules.readLaneList")(function* (file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(file).pipe(Effect.mapError(readFailure(file)))
  const json = yield* Effect.try({
    try: () => JSON.parse(text) as unknown,
    catch: (cause) => new LaneListCorrupt({ file, reason: `not JSON: ${String(cause)}` })
  })
  return yield* decodeLaneFile(json).pipe(
    Effect.mapError((issue) => new LaneListCorrupt({ file, reason: String(issue) }))
  )
})

/** Why `entry` of class `cls` refuses to load, given the rules `claimed` earlier in its file; `undefined` where it loads. */
const refusalOf = (
  lane: string,
  cls: "accepted" | "known_bug",
  entry: Entry,
  claimed: ReadonlySet<string>,
  known: ReadonlySet<string>
): string | undefined => {
  if (!known.has(entry.rule)) return `lane ${lane} lists rule ${entry.rule}, which the registry does not define`
  if (claimed.has(entry.rule)) return `rule ${entry.rule} appears in both classes of one file`
  if (cls === "known_bug" && entry.ticket === undefined) {
    return `a known_bug entry must name the ticket it waits on (${entry.rule})`
  }
  if (cls === "accepted" && entry.ticket !== undefined) {
    return `an accepted entry waits on nothing — drop the ticket (${entry.rule})`
  }
  return undefined
}

/**
 * Load the list `lane` and its `extends` chain from `source`, nearest lane
 * first: a rule a nearer list already classified keeps that class, so a
 * nearer lane OVERRIDES a further one on the same rule.
 */
export const loadLanePolicy = Effect.fn("DeltaRules.loadLanePolicy")(function* (lane: string, source: ListSource) {
  const accepted = new Map<string, Entry>()
  const knownBug = new Map<string, Entry>()
  const seen = new Set<string>()
  let current: string | undefined = lane
  while (current !== undefined) {
    const file: string = yield* listFile(current, source.dir, source.beside)
    if (seen.has(current)) {
      return yield* new LaneListCorrupt({ file, reason: `extends cycle at ${current}` })
    }
    seen.add(current)
    const parsed = yield* readList(file)
    const claimed = new Set<string>()
    for (const [cls, list] of [
      ["accepted", parsed.accepted],
      ["known_bug", parsed.known_bug]
    ] as const) {
      for (const entry of list) {
        const reason = refusalOf(parsed.lane, cls, entry, claimed, source.known)
        if (reason !== undefined) return yield* new LaneListCorrupt({ file, reason })
        claimed.add(entry.rule)
        if (accepted.has(entry.rule) || knownBug.has(entry.rule)) continue
        if (cls === "accepted") accepted.set(entry.rule, entry)
        else knownBug.set(entry.rule, entry)
      }
    }
    current = parsed.extends
  }
  return { lane, accepted, knownBug } satisfies LanePolicy
})

/**
 * One probe classified: the first of `rules` that names it, and the class
 * `policy` lists that rule in. No policy lists nothing: every named probe is
 * `unlisted`.
 */
export const classify = <F, S extends string>(
  policy: LanePolicy | undefined,
  rules: ReadonlyArray<DeltaRule<F, S>>,
  probe: Probe.Probe,
  facts: F
): Classifier.Classification => {
  const rule = matchRule(rules, probe, facts)
  if (rule === undefined) return { class: "unknown", rule: "", ticket: "" }
  if (policy?.accepted.has(rule.id) === true) return { class: "accepted", rule: rule.id, ticket: "" }
  const bug = policy?.knownBug.get(rule.id)
  if (bug !== undefined) return { class: "known-bug", rule: rule.id, ticket: bug.ticket ?? "" }
  return { class: "unlisted", rule: rule.id, ticket: "" }
}

/** The classes an enforcing run passes on. */
export const passesEnforcing = (cls: Confrontation.RecordClass): boolean =>
  cls === "accepted" || cls === "known-bug"
