/**
 * The queue's chip counts over the GROUPS the statement aggregates: one per
 * distinct answer of the pool — the rows the scope and the capture substring
 * keep — with how many calls give it. Pure: the store hands the groups, the
 * page counts here.
 *
 * A query has a SCOPE (which rows are on the queue at all), a capture
 * substring, and six CHIP GROUPS — the rule, the expected message, the
 * refusal, the outcome, the feature, the source fault. The source fault is
 * information beside the outcome: a call answers to its rule passed or failed,
 * and it moves no outcome count. Each takes one INCLUDE (the row must answer to the
 * value) and any number of EXCLUDES (a row answering to any of them is
 * hidden). The store narrows the page by all of them; the chips are counted
 * over the pool minus the excludes, never the includes, so a chip's count
 * always answers "of what is left, how many calls died on X", and clicking
 * one include never zeroes the others. An excluded value's own chip answers
 * "how many calls does this exclusion hide": its count is taken with that one
 * exclusion lifted, the others still applied. A call refused under several
 * reasons, or carrying several features, counts under, answers to, and is
 * hidden by, each; so does a case whose main error is another than `bad-body`
 * while it carries a failing body row, under its rule and
 * under `bad-body`.
 */
import type { RowKind } from "./calls.js"
import { MainError } from "@sip/contracts"

export const SCOPES = ["red", "all", "passed", "failed", "skipped", "refused"] as const
export type Scope = (typeof SCOPES)[number]

/** Failed: the SUT's own failing calls — the SIP reader's work; a cut refusal is the pipeline's. */
export const DEFAULT_SCOPE: Scope = "failed"

/** The row kinds of the queue, in the order the outcome chips list them. */
export const OUTCOMES = ["refused", "failed", "passed", "skipped"] as const
export type Outcome = (typeof OUTCOMES)[number]

/** The chip groups: the key an include is named by and an exclusion set is kept under. */
export const GROUPS = ["rule", "expected", "refusal", "outcome", "feature", "source"] as const
export type ChipGroup = (typeof GROUPS)[number]

/** Per chip group, the values whose rows are hidden. */
export type Excludes = Partial<Record<ChipGroup, ReadonlyArray<string>>>

export interface Query {
  readonly scope?: Scope | undefined
  /** A case-insensitive substring of the capture name, read literally. */
  readonly capture?: string | undefined
  /** Exact `mainError.rule` of a case row (an RFC rule id or a failure tag alike), or `bad-body` for any case carrying a failing SDP row. */
  readonly rule?: string | undefined
  /** Exact `mainError.expected` of a case row. */
  readonly expected?: string | undefined
  /** Exact refusal reason of a refused row. */
  readonly refusal?: string | undefined
  /** Exact outcome of a row. */
  readonly outcome?: Outcome | undefined
  /** One feature the row's document carries (`case_documents.features`). */
  readonly feature?: string | undefined
  /** The rule of the source fault a case carries (`CallRow.sourceFault`), passed or failed. */
  readonly source?: string | undefined
  /** The values hidden per chip group; a row answering to any of them is off the queue and out of every count. */
  readonly exclude?: Excludes | undefined
}

/**
 * One distinct answer of the pool and how many calls give it: a case row's
 * outcome and main error (its kind, rule, citation and expected message), a
 * refused row's reasons. A refused row answers to no rule and no expected
 * message; a case answers to no reason.
 */
export interface Group {
  readonly kind: RowKind
  readonly outcome: Outcome
  readonly errorKind: string | null
  readonly rule: string | null
  readonly rfc: string | null
  readonly expected: string | null
  readonly reasons: ReadonlyArray<string>
  /** The features of the case's document; empty for a refused call. */
  readonly features: ReadonlyArray<string>
  /** The rule of the source fault the case carries; absent where it carries none. */
  readonly source?: string
  /** Set where the case carries a body row no rule accepts. */
  readonly badBody?: true
  readonly count: number
}

export interface Count {
  readonly value: string
  readonly count: number
}

export interface RuleCount extends Count {
  /** The citation the rule's findings carry; null where none of them cites one. */
  readonly rfc: string | null
}

export interface Facets {
  /** The charged RFC rules (`mainError.kind` `rfc`), with the citation they carry. */
  readonly rules: ReadonlyArray<RuleCount>
  /** The failure tags of every other named failure of a case, the interpreter's refusal and `bad-body` included. */
  readonly failures: ReadonlyArray<Count>
  readonly expected: ReadonlyArray<Count>
  readonly refusals: ReadonlyArray<Count>
  /** The outcomes, in `OUTCOMES` order. */
  readonly outcomes: ReadonlyArray<Count>
  /** The features, independent of the outcome; a call counts once under each it carries. */
  readonly features: ReadonlyArray<Count>
  /** The source faults' rules, passed or failed: information, which no outcome count reads. */
  readonly sources: ReadonlyArray<Count>
  /** Distinct captures among the rows the scope, the capture filter and the excludes keep. */
  readonly captures: number
}

/** The chips alone: `captures` is the statement's own count. */
export type Chips = Omit<Facets, "captures">

const rfcRuleOf = (group: Group): string | null => (group.errorKind === "rfc" ? group.rule : null)

const failureOf = (group: Group): string | null => (group.errorKind === "rfc" ? null : group.rule)

/**
 * The rule values a case answers to: its main error's rule, and `bad-body`
 * where it carries a failing body row its main error does not
 * already name.
 */
export const rulesOf = (rule: string | null, badBody: boolean): ReadonlyArray<string> => [
  ...(rule === null ? [] : [rule]),
  ...(badBody && rule !== MainError.BAD_BODY ? [MainError.BAD_BODY] : [])
]

const failuresOf = (group: Group): ReadonlyArray<string> => rulesOf(failureOf(group), group.badBody === true)

const one = (valueOf: (group: Group) => string | null) => (group: Group): ReadonlyArray<string> => {
  const value = valueOf(group)
  return value === null ? [] : [value]
}

/** The values a group answers to, per chip group. */
const valuesOf: Readonly<Record<ChipGroup, (group: Group) => ReadonlyArray<string>>> = {
  rule: (group) => rulesOf(group.rule, group.badBody === true),
  expected: one((group) => group.expected),
  refusal: (group) => group.reasons,
  outcome: (group) => [group.outcome],
  feature: (group) => group.features,
  source: (group) => (group.source === undefined ? [] : [group.source])
}

/** Whether any chip group hides the group: one of its values of that chip group is excluded. */
const excluded = (group: Group, exclude: Excludes): boolean =>
  GROUPS.some((chip) => {
    const hidden = exclude[chip]
    return hidden !== undefined && hidden.length > 0 && valuesOf[chip](group).some((value) => hidden.includes(value))
  })

/** The groups the excludes keep: what every chip counts over. */
const kept = (groups: ReadonlyArray<Group>, exclude: Excludes): ReadonlyArray<Group> =>
  groups.filter((group) => !excluded(group, exclude))

/** The excludes with one chip group's value lifted, the rest as they are. */
const lifting = (exclude: Excludes, chip: ChipGroup, value: string): Excludes => ({
  ...exclude,
  [chip]: (exclude[chip] ?? []).filter((one) => one !== value)
})

const byCount = (a: Count, b: Count): number => b.count - a.count || (a.value < b.value ? -1 : a.value > b.value ? 1 : 0)

/** One count per distinct value, most first; a group counts its calls once under each value it names. */
const counted = (
  groups: ReadonlyArray<Group>,
  valuesOf: (group: Group) => ReadonlyArray<string>
): ReadonlyArray<Count> => {
  const counts = new Map<string, number>()
  for (const group of groups) {
    for (const value of valuesOf(group)) counts.set(value, (counts.get(value) ?? 0) + group.count)
  }
  return [...counts].map(([value, count]) => ({ value, count })).sort(byCount)
}

/**
 * What each of a chip group's excluded values hides, under the chip's own
 * reading of the group (`valueOf`): the calls carrying it once that one
 * exclusion is lifted. A value hiding nothing under this reading is not
 * listed — a failure tag is no rule — and the page pins it at zero.
 */
const hidden = (
  groups: ReadonlyArray<Group>,
  exclude: Excludes,
  chip: ChipGroup,
  valueOf: (group: Group) => ReadonlyArray<string>
): ReadonlyArray<Count> =>
  (exclude[chip] ?? [])
    .map((value) => ({
      value,
      count: counted(kept(groups, lifting(exclude, chip, value)), valueOf).find((one) => one.value === value)?.count ?? 0
    }))
    .filter((entry) => entry.count > 0)

/** The outcome counts in the fixed `OUTCOMES` order, so the chips never move. */
const byOutcome = (a: Count, b: Count): number =>
  OUTCOMES.indexOf(a.value as Outcome) - OUTCOMES.indexOf(b.value as Outcome)

/**
 * The chip counts over the pool's groups with the excluded values hidden,
 * plus what each excluded value hides; the includes do not narrow them. Only
 * the query's excludes read here: the scope and the capture are the pool's.
 */
export const facets = (groups: ReadonlyArray<Group>, query: Query): Chips => {
  const exclude = query.exclude ?? {}
  const population = kept(groups, exclude)
  // A citation is the rule's, so an excluded rule's chip cites it too; a rule
  // cited two ways carries the lexically first, whatever order the groups come in.
  const citations = new Map<string, string>()
  for (const group of kept(groups, { ...exclude, rule: [] })) {
    const rule = rfcRuleOf(group)
    if (rule === null || group.rfc === null) continue
    const held = citations.get(rule)
    if (held === undefined || group.rfc < held) citations.set(rule, group.rfc)
  }
  const chip = (
    key: ChipGroup,
    valueOf: (group: Group) => ReadonlyArray<string>,
    order: (a: Count, b: Count) => number = byCount
  ): ReadonlyArray<Count> => [...counted(population, valueOf), ...hidden(groups, exclude, key, valueOf)].sort(order)
  return {
    rules: chip("rule", one(rfcRuleOf)).map((entry) => ({ ...entry, rfc: citations.get(entry.value) ?? null })),
    failures: chip("rule", failuresOf),
    expected: chip("expected", one((group) => group.expected)),
    refusals: chip("refusal", valuesOf.refusal),
    outcomes: chip("outcome", valuesOf.outcome, byOutcome),
    features: chip("feature", valuesOf.feature),
    sources: chip("source", valuesOf.source)
  }
}
