/**
 * The predicates that narrow a work list to a STATED population, rather than to
 * a curated file: how long the capture is, whether it already passes on another
 * lane, and a sample of what is left.
 *
 * Separate from `universe.ts` because the two answer different questions. The
 * universe says where captures come from; this says which of them qualify. Both
 * predicates read facts the run database already holds, so a population is
 * named by a command line and recomputed on every invocation — a list built by
 * hand goes stale the moment a capture is re-cut or re-run.
 */
import { Cuts, Model, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as crypto from "node:crypto"
import type { Paths } from "./paths.js"
import type { Selection, Work } from "./universe.js"

/**
 * Why these NAMED captures cannot be worked on, or `null`.
 *
 * A capture is a pcap basename keyed by the artifact store, and nothing
 * downstream checks that: a name nobody owns opens a run row, that row joins
 * the `--all` universe for good, and every later sweep re-runs it to the same
 * `no flows doc` error. So a named capture the store does not hold refuses the
 * invocation before any row opens.
 *
 * A name must also BE a basename. `path.join` would resolve `../` out of the
 * store and an absolute path into a second root, so a name carrying either is
 * refused on its shape rather than on whether that escape happens to exist.
 */
const unknownCaptures = Effect.fn("Population.unknownCaptures")(function*(paths: Paths, selection: Selection) {
  if (selection.universe !== "named" || selection.positional.length === 0) return null
  const fs = yield* FileSystem.FileSystem
  const held = new Set(yield* fs.readDirectory(paths.corpus).pipe(Effect.orElseSucceed(() => [] as Array<string>)))
  const unknown = selection.positional.filter((name) => !held.has(name))
  if (unknown.length === 0) return null
  // A miss is hand-entry damage — a pasted path, a dropped suffix, a single
  // character gone from the middle of a name — so name the one store entry the
  // typo is a near miss for where exactly one qualifies.
  const near = (name: string): string => {
    const stem = name.slice(0, 20)
    const hits = [...held].filter((entry) => entry.startsWith(stem))
    return hits.length === 1 ? ` — did you mean ${hits[0]}?` : ""
  }
  return unknown.map((name) => `${JSON.stringify(name)} names no capture in ${paths.corpus}${near(name)}`).join("; ")
})


/** A bound the invocation stated: given at all, and a positive number when it was. */
const bound = (name: string, value: number | undefined): string | null =>
  value === undefined || (Number.isFinite(value) && value > 0)
    ? null
    : `--${name} takes a positive number`

/**
 * Why a case set cannot be honoured, or `null`: a case id names a call of ONE
 * capture, so `--case` over two captures or over a derived universe would name
 * a case of which capture? And `--case` states the calls to replay while
 * `--max-span` rules which calls replay: two answers to one question.
 */
const caseSet = (selection: Selection): string | null => {
  if (selection.cases.length === 0) return null
  if (selection.universe !== "named") {
    return `--case names the calls of one capture; it cannot be combined with --${selection.universe}`
  }
  if (selection.positional.length !== 1) {
    return `--case names the calls of one capture; name exactly one (got ${selection.positional.length})`
  }
  if (selection.maxSpanSeconds !== undefined) {
    return "--case names the calls to replay; it cannot be combined with --max-span, which rules them by span"
  }
  return null
}

/**
 * Why this selection cannot be honoured, or `null`.
 *
 * Every predicate here fails SILENTLY when it is mis-stated — a `NaN` bound
 * excludes everything and a lane name nobody has run excludes everything — and
 * an empty work list reads as "the corpus has nothing to do" rather than as a
 * typo. So a stated predicate that names nothing refuses the invocation instead.
 */
export const refusal = Effect.fn("Population.refusal")(function*(paths: Paths, selection: Selection) {
  const set = caseSet(selection)
  if (set !== null) return set
  const named = yield* unknownCaptures(paths, selection)
  if (named !== null) return named
  const stated = bound("max-span", selection.maxSpanSeconds) ??
    bound("sample", selection.sampleSize) ??
    bound("limit", selection.limit)
  if (stated !== null) return stated
  if (selection.greenOnLane === undefined) return null
  const runs = yield* Runs.Service
  const lanes = yield* runs.lanes
  if (lanes.some((lane) => lane.lane === selection.greenOnLane)) return null
  const known = lanes.map((lane) => lane.lane).join(", ")
  return `--green-on ${selection.greenOnLane} names no lane the run record knows (${known})`
})

/**
 * The calls of `captures` that span no more than `seconds`, per capture: a
 * capture whose every standing case is under the bound replays `full`, one
 * with some cases under it replays a `cases` set naming exactly those, and
 * one with none is dropped. Answered in the order asked.
 *
 * A case the cut store states no span for is out: the bound is a claim about
 * wall-clock cost, and a call that cannot support the claim must not ride on
 * it. A capture whose cases all state none is dropped, not replayed whole.
 */
export const withinSpan = Effect.fn("Population.withinSpan")(function*(
  captures: ReadonlyArray<string>,
  seconds: number,
  prepId: number | undefined
) {
  const cuts = yield* Cuts.Service
  const spans = yield* cuts.caseSpans(prepId)
  const ceiling = seconds * 1000
  const kept: Array<Work> = []
  for (const capture of captures) {
    const cases = spans.get(capture)
    if (cases === undefined) continue
    const under = [...cases].filter(([, span]) => span !== null && span <= ceiling).map(([id]) => id)
    if (under.length === 0) continue
    kept.push({ capture, set: under.length === cases.size ? { kind: "full" } : { kind: "cases", cases: under } })
  }
  return kept as ReadonlyArray<Work>
})

/**
 * Captures whose LATEST run on `lane` was green.
 *
 * Latest, not never-red: a stability filter over every run a capture ever had
 * asks whether a lane has been steady, which is a different question and — on a
 * lane nobody has swept many times — cuts a corpus to a handful. What this asks
 * is what the corpus stands at now.
 */
export const greenOn = Effect.fn("Population.greenOn")(function*(
  captures: ReadonlyArray<string>,
  lane: string
) {
  const runs = yield* Runs.Service
  const standings = yield* runs.standings(lane)
  const green = new Set(
    standings.filter((standing) => Model.isGreen(standing)).map((standing) => standing.capture)
  )
  return captures.filter((capture) => green.has(capture))
})

/** Where a capture falls in the sample order: a hash of its name, and nothing else. */
const rank = (capture: string): string => crypto.createHash("md5").update(capture).digest("hex")

/**
 * `size` captures drawn from `captures`, the same ones every time.
 *
 * The draw is keyed on the capture NAME, so it is stable across sweeps and
 * across re-cuts — a re-run compares one population against itself. That is
 * what separates it from `--limit`, which truncates a sorted list and so biases
 * a sample toward whichever end of the corpus the sort put first. The answer
 * comes back in capture order, so the work list runs in the order every other
 * selection runs in.
 */
export const sample = (
  captures: ReadonlyArray<string>,
  size: number
): ReadonlyArray<string> => {
  if (size >= captures.length) return captures
  const ranked = captures.map((capture) => ({ capture, rank: rank(capture) }))
  ranked.sort((left, right) => (left.rank < right.rank ? -1 : left.rank > right.rank ? 1 : 0))
  return ranked.slice(0, size).map((entry) => entry.capture).sort()
}
