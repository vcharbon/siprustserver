/**
 * WHICH captures a phase works on: the `--all` universe, and the work list the
 * command line asked for.
 *
 * The POPULATION is the corpus's own list of captures — the store's
 * `population` table, which an operator adds pcaps to — and it is the
 * no-regression corpus. It is NOT the whole story: the captures anybody has
 * replayed are largely disjoint from it, so the universe is the union of the
 * population and every capture the run database already knows, or a capture
 * that once ran green would fall out of every lane and its regression would go
 * unseen.
 */
import { Population, Queue, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as crypto from "node:crypto"
import type { Paths } from "./paths.js"
import { greenOn, sample, withinSpan } from "./population.js"
import { read as readOverlay } from "./triage-overlay.js"

/** The population: the captures the store holds as the corpus. */
export const standing = Effect.flatMap(Population.Service, (population) => population.captures)

/** The population and every capture the run record knows, which is `--all`. */
export const captures = Effect.fn("Universe.captures")(function*(recorded: Iterable<string>) {
  const all = new Set(recorded)
  for (const capture of yield* standing) all.add(capture)
  return [...all].sort() as ReadonlyArray<string>
})

/**
 * Which work list this invocation took: the whole universe, the population
 * alone, the queue, or a named list.
 *
 * `standing` is the only one that does not drift. `all` grows whenever any
 * capture outside the population gets a run row, so a count taken over it
 * cannot be PINNED — a census is a property of a named population, and this is
 * the one the corpus states.
 */
export type Universe = "all" | "standing" | "known" | "named"

export interface Selection {
  readonly universe: Universe
  readonly positional: ReadonlyArray<string>
  readonly onlyNew: boolean
  /**
   * The population predicates (`population.ts`), applied to whatever the
   * universe named. Each is absent where the invocation did not state it, so a
   * selection that names none is the whole universe it always was.
   */
  readonly maxSpanSeconds: number | undefined
  readonly greenOnLane: string | undefined
  readonly sampleSize: number | undefined
  readonly limit: number | undefined
  /**
   * The case ids `--case` stated: empty where it stated none, so the set is
   * whatever the bound leaves or `full`; otherwise these ids of the ONE named
   * capture, a `cases` run of those calls.
   */
  readonly cases: ReadonlyArray<string>
}

/**
 * The case set one capture's run names (`runs.set_kind`): every case
 * the pinned prep cut for it, or these ids of it alone.
 */
export type CaseSet =
  | { readonly kind: "full" }
  | { readonly kind: "cases"; readonly cases: ReadonlyArray<string> }

/** A work item replayed as a cases set: what a span bound kept in part, or `--case` named. */
export interface PartialWork extends Work {
  readonly set: Extract<CaseSet, { kind: "cases" }>
}

export const isPartial = (work: Work): work is PartialWork => work.set.kind === "cases"

const FULL: CaseSet = { kind: "full" }

/** One unit of a work list: a capture, and the set of its calls this invocation replays. */
export interface Work {
  readonly capture: string
  readonly set: CaseSet
}

/**
 * The captures a selection names, de-duplicated, each with its case set: one
 * capture is one unit of work, and a repeat would open two rows for it and
 * finish both against the second one's bundle.
 *
 * The universe is named first and the population predicates narrow it, in the
 * order stated: span, then green-on-a-lane, then the sample. The sample must
 * come last or it would draw from a population the predicates go on to shrink,
 * and `--sample 1000` would answer with fewer than 1000. The span bound is the
 * one predicate that rules per CALL: a capture it keeps in part is a `cases`
 * set of the calls under the bound; `--case` names the set outright, and the
 * two are never stated together (`Population.refusal`).
 */
export const workList = Effect.fn("Universe.workList")(function*(
  paths: Paths,
  lane: string,
  selection: Selection,
  prepId?: number
) {
  const runs = yield* Runs.Service
  const named = yield* Effect.gen(function*() {
    if (selection.universe === "known") {
      const standings = yield* runs.standings(lane)
      const { registry, tickets } = yield* readOverlay(paths.registry)
      return Queue.knownCaptures({ standings, registry, tickets })
    }
    if (selection.universe === "standing") return yield* standing
    if (selection.universe === "all") {
      // Every capture the lane ran, whoever ran it: "new" is a capture the lane never ran at all.
      const seen = new Set(yield* runs.ranOn(lane))
      const all = yield* captures(seen)
      return selection.onlyNew ? all.filter((capture) => !seen.has(capture)) : all
    }
    return selection.positional
  })
  let list: ReadonlyArray<string> = [...new Set(named)]
  const sets = new Map<string, CaseSet>()
  if (selection.maxSpanSeconds !== undefined) {
    const within = yield* withinSpan(list, selection.maxSpanSeconds, prepId)
    for (const work of within) sets.set(work.capture, work.set)
    list = within.map((work) => work.capture)
  }
  if (selection.greenOnLane !== undefined) list = yield* greenOn(list, selection.greenOnLane)
  if (selection.sampleSize !== undefined) list = sample(list, selection.sampleSize)
  if (selection.limit !== undefined) list = list.slice(0, selection.limit)
  const stated: CaseSet = selection.cases.length === 0 ? FULL : { kind: "cases", cases: selection.cases }
  return list.map((capture) => ({ capture, set: sets.get(capture) ?? stated } satisfies Work)) as ReadonlyArray<Work>
})

/** Past this many named captures, a stated selection names them by count and content hash. */
const NAMED_SHOWN = 5

const namedList = (captures: ReadonlyArray<string>): string =>
  captures.length <= NAMED_SHOWN
    ? captures.join(" ")
    : `${captures.length} named captures #${
      crypto.createHash("sha256").update([...captures].sort().join("\n")).digest("hex").slice(0, 12)
    }`

/**
 * The population a selection states, as the command line would state it: the
 * universe's switch or the named captures (a long list by its count and
 * content hash), then every predicate in the order `workList` applies them.
 * Two sweeps whose lines differ swept two populations.
 */
export const stated = (selection: Selection): string =>
  [
    selection.universe === "named" ? namedList(selection.positional) : `--${selection.universe}`,
    selection.onlyNew ? "--only-new" : "",
    selection.maxSpanSeconds === undefined ? "" : `--max-span ${selection.maxSpanSeconds}`,
    selection.greenOnLane === undefined ? "" : `--green-on ${selection.greenOnLane}`,
    selection.sampleSize === undefined ? "" : `--sample ${selection.sampleSize}`,
    selection.limit === undefined ? "" : `--limit ${selection.limit}`,
    ...selection.cases.map((id) => `--case ${id}`)
  ].filter((part) => part !== "").join(" ")

/** One work item as a dry-run line: the capture, and the size of a cases set. */
export const label = (work: Work): string =>
  isPartial(work) ? `${work.capture}  (${work.set.cases.length}-case set)` : work.capture
