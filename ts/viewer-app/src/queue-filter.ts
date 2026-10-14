/**
 * The chip filter of the triage queue, as the page holds it and as the URL
 * carries it. Six GROUPS — rule, expected, refusal, outcome, feature, source (a
 * source fault's rule, information beside the outcome) — each with one
 * INCLUDE (the URL key is the group's name) and a set of EXCLUDES (the key
 * `not-<group>`, repeated per value). Pure: the page reads its URL through
 * `readFilter`, writes it through `filterParams`, and a click moves one chip
 * through `cycle`; the server applies the same keys.
 */
import type { QueueParams } from "./router.ts"

/** The chip groups, in the order the chip row lists them. */
export const GROUPS = ["rule", "expected", "refusal", "outcome", "feature", "source"] as const
export type Group = (typeof GROUPS)[number]

export interface ChipFilter {
  /** The one included value per group; "" = none. */
  readonly include: Readonly<Record<Group, string>>
  /** The hidden values per group, in the order they were excluded. */
  readonly exclude: Readonly<Record<Group, ReadonlyArray<string>>>
}

/** What a chip is to the filter: not part of it, its group's include, or one of its group's excludes. */
export type ChipState = "none" | "include" | "exclude"

/** The URL key a group's excludes repeat under. */
export const excludeKey = (group: Group): `not-${Group}` => `not-${group}`

const NONE = { rule: "", expected: "", refusal: "", outcome: "", feature: "", source: "" } as const
const NOTHING = { rule: [], expected: [], refusal: [], outcome: [], feature: [], source: [] } as const

export const EMPTY: ChipFilter = { include: NONE, exclude: NOTHING }

const distinct = (values: ReadonlyArray<string>): ReadonlyArray<string> =>
  [...new Set(values.filter((value) => value !== ""))]

const over = <A>(make: (group: Group) => A): Readonly<Record<Group, A>> =>
  Object.fromEntries(GROUPS.map((group) => [group, make(group)])) as Record<Group, A>

/**
 * The filter a query string states: a blank include is none, a repeated
 * exclude is one, and an include its group also excludes is dropped — the
 * exclusion wins, as it does on the server.
 */
export const readFilter = (query: URLSearchParams): ChipFilter => {
  const exclude = over((group) => distinct(query.getAll(excludeKey(group))))
  return {
    include: over((group) => {
      const value = query.get(group) ?? ""
      return exclude[group].includes(value) ? "" : value
    }),
    exclude
  }
}

/** The filter's share of the query string, for `setQuery` and the API call alike. */
export const filterParams = (filter: ChipFilter): Pick<QueueParams, Group | `not-${Group}`> => ({
  rule: filter.include.rule,
  expected: filter.include.expected,
  refusal: filter.include.refusal,
  outcome: filter.include.outcome,
  feature: filter.include.feature,
  source: filter.include.source,
  "not-rule": filter.exclude.rule,
  "not-expected": filter.exclude.expected,
  "not-refusal": filter.exclude.refusal,
  "not-outcome": filter.exclude.outcome,
  "not-feature": filter.exclude.feature,
  "not-source": filter.exclude.source
})

/** One line naming the filter whole, for a cache key. */
export const filterKey = (filter: ChipFilter): string => JSON.stringify(filterParams(filter))

export const stateOf = (filter: ChipFilter, group: Group, value: string): ChipState =>
  filter.exclude[group].includes(value) ? "exclude" : filter.include[group] === value ? "include" : "none"

const withInclude = (filter: ChipFilter, group: Group, value: string): ChipFilter => ({
  ...filter,
  include: { ...filter.include, [group]: value }
})

const withExcludes = (filter: ChipFilter, group: Group, values: ReadonlyArray<string>): ChipFilter => ({
  ...filter,
  exclude: { ...filter.exclude, [group]: values }
})

/**
 * A click on the chip row: none → include → exclude → none. An include
 * replaces the group's include; an exclude joins the group's excludes.
 */
export const cycle = (filter: ChipFilter, group: Group, value: string): ChipFilter => {
  switch (stateOf(filter, group, value)) {
    case "none":
      return withInclude(filter, group, value)
    case "include":
      return withExcludes(withInclude(filter, group, ""), group, [...filter.exclude[group], value])
    case "exclude":
      return withExcludes(filter, group, filter.exclude[group].filter((one) => one !== value))
  }
}

/** A click on a row's chip: the value becomes the group's include, and leaves its excludes. */
export const select = (filter: ChipFilter, group: Group, value: string): ChipFilter =>
  withExcludes(
    withInclude(filter, group, value),
    group,
    filter.exclude[group].filter((one) => one !== value)
  )
