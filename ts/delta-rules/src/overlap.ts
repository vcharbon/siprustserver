/**
 * The overlap graph (ADR-0062): every pair of rules that name one probe
 * together, read off full match sets. A pair is (winner, shadowed), the winner
 * the earlier in match order; a match set of n rules gives every one of its
 * n(n-1)/2 pairs. Each pair carries both rules' sections, how many probes of
 * each source showed it, and the lanes on which the two rules' classes differ,
 * where the order decides pass or fail. Its golden form ({@link goldenOf})
 * keeps the pairs, their sections and sources, never a count: a deployment
 * checks it in, and a test that regenerates it from the deployment's own
 * sources fails, naming the pair, when an edit moves a winner.
 */
import * as Schema from "effect/Schema"
import type { LanePolicy } from "./lane-lists.js"

/** Where a match set came from. */
export type Source = "fixtures" | "fixture-mutants" | "archive" | "archive-mutants"

/** Every source, in the order a golden pair lists them. */
export const SOURCES: ReadonlyArray<Source> = ["archive", "archive-mutants", "fixture-mutants", "fixtures"]

/** The sources a test regenerates from the committed fixtures alone. */
export const FIXTURE_SOURCES: ReadonlyArray<Source> = ["fixtures", "fixture-mutants"]

/** One co-matching pair and what the graph knows of it. */
export interface Edge<S extends string = string> {
  readonly winner: string
  readonly shadowed: string
  readonly sections: readonly [S, S]
  readonly counts: Readonly<Partial<Record<Source, number>>>
  /** The lanes on which the winner's class and the shadowed rule's class differ. */
  readonly differOn: ReadonlyArray<string>
}

/** The golden form of one pair: no count. */
export interface GoldenPair<S extends string = string> {
  readonly winner: string
  readonly shadowed: string
  readonly sections: readonly [S, S]
  readonly sources: ReadonlyArray<Source>
}

/** Counts by pair key (`winner>shadowed`), by source. */
export type Tally = Map<string, Map<Source, number>>

/** The graph over one deployment's rules, in their match order. */
export interface OverlapGraph<S extends string> {
  /** Every (winner, shadowed) pair of one match set, ids the rules do not define left out. */
  readonly pairsOf: (matches: ReadonlyArray<string>) => ReadonlyArray<readonly [string, string]>
  /** `tally` with one match set of `source`, seen `count` times, counted in. */
  readonly tallyMatches: (tally: Tally, source: Source, matches: ReadonlyArray<string>, count?: number) => void
  /** The graph a tally states, pairs in match order of winner then shadowed. */
  readonly edgesOf: (tally: Tally, policies: ReadonlyArray<LanePolicy>) => ReadonlyArray<Edge<S>>
}

/** A rule's class on a lane, its ticket left out. */
const classOfRule = (policy: LanePolicy, id: string): string =>
  policy.accepted.has(id) ? "accepted" : policy.knownBug.has(id) ? "known-bug" : "unlisted"

/** The overlap graph of `rules`, read in the order given. */
export const overlapGraph = <S extends string>(
  rules: ReadonlyArray<{ readonly id: string; readonly section: S }>
): OverlapGraph<S> => {
  const order = new Map(rules.map((rule, i) => [rule.id, i]))
  const sectionOf = new Map(rules.map((rule) => [rule.id, rule.section]))
  const orderOf = (id: string): number => order.get(id) ?? Number.MAX_SAFE_INTEGER

  const pairsOf = (matches: ReadonlyArray<string>): ReadonlyArray<readonly [string, string]> => {
    const named = matches.filter((id) => order.has(id))
    return named.flatMap((winner, i) => named.slice(i + 1).map((shadowed) => [winner, shadowed] as const))
  }

  const tallyMatches = (tally: Tally, source: Source, matches: ReadonlyArray<string>, count = 1): void => {
    for (const [winner, shadowed] of pairsOf(matches)) {
      const key = `${winner}>${shadowed}`
      const bySource = tally.get(key) ?? new Map<Source, number>()
      bySource.set(source, (bySource.get(source) ?? 0) + count)
      tally.set(key, bySource)
    }
  }

  const edgesOf = (tally: Tally, policies: ReadonlyArray<LanePolicy>): ReadonlyArray<Edge<S>> =>
    [...tally.entries()]
      .flatMap(([key, bySource]): ReadonlyArray<Edge<S>> => {
        const [winner = "", shadowed = ""] = key.split(">")
        const won = sectionOf.get(winner)
        const lost = sectionOf.get(shadowed)
        if (won === undefined || lost === undefined) return []
        return [{
          winner,
          shadowed,
          sections: [won, lost],
          counts: Object.fromEntries(bySource),
          differOn: policies.filter((p) => classOfRule(p, winner) !== classOfRule(p, shadowed)).map((p) => p.lane)
        }]
      })
      .sort((a, b) => (orderOf(a.winner) - orderOf(b.winner)) || (orderOf(a.shadowed) - orderOf(b.shadowed)))

  return { pairsOf, tallyMatches, edgesOf }
}

/** The golden form of a graph: each pair, its sections and the sources that showed it. */
export const goldenOf = <S extends string>(edges: ReadonlyArray<Edge<S>>): ReadonlyArray<GoldenPair<S>> =>
  edges.map((edge) => ({
    winner: edge.winner,
    shadowed: edge.shadowed,
    sections: edge.sections,
    sources: SOURCES.filter((source) => edge.counts[source] !== undefined)
  }))

/** The part of a golden graph `sources` alone can show: pairs they showed, with those sources only. */
export const restrictedTo = <S extends string>(
  pairs: ReadonlyArray<GoldenPair<S>>,
  sources: ReadonlyArray<Source>
): ReadonlyArray<GoldenPair<S>> =>
  pairs.flatMap((pair) => {
    const kept = pair.sources.filter((s) => sources.includes(s))
    return kept.length === 0 ? [] : [{ ...pair, sources: kept }]
  })

const SourceSchema = Schema.Literals(["fixtures", "fixture-mutants", "archive", "archive-mutants"])

/** The schema of an overlap golden file whose sections are `sections`. */
export const overlapFile = <const S extends ReadonlyArray<string>>(sections: S) => {
  const section = Schema.Literals(sections)
  return Schema.Struct({
    pairs: Schema.Array(Schema.Struct({
      winner: Schema.String,
      shadowed: Schema.String,
      sections: Schema.Tuple([section, section]),
      sources: Schema.Array(SourceSchema)
    }))
  })
}
