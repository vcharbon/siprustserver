/**
 * What a triage command line CAME TO: the roots it states over the
 * deployment's (`Roots`), and the parsed triage flags read into the options
 * the phases share, each bound's own arithmetic applied once here rather than
 * in every phase.
 *
 * A knob the line leaves unstated takes, on a line that drives a lane, the
 * request row's where there is one, then the lane's default, then the generic
 * one (`@sip/corpus` `knob-resolution.ts`). A cut drives no lane and takes the
 * generic ones; a line that cuts and then replays shares its concurrency and
 * batch with the cut, so only its pace is the lane's.
 */
import { Roots } from "@sip/corpus"
import { DEFAULT_CLEANUP, DEFAULT_MEMORY_MAX } from "@sip/corpus/knobs"
import type { Options } from "@sip/corpus/options"
import type { Overrides } from "@sip/corpus/paths"
import * as KnobResolution from "@sip/corpus/replay/knob-resolution"
import type { Selection } from "@sip/corpus/universe"
import type { Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { type PathInput, patienceOf, type TriageInput } from "./flags.js"

export const USAGE_TAIL = "<capture>.pcap.gz... | --known | --all | --standing " +
  "[--only-new] [--max-span SECONDS] [--green-on LANE] [--sample N] " +
  "[--limit N] [--case ID]... [--dry-run] [--lane L] [--batch N] [--concurrency N] " +
  "[--starts-per-second N] [--memory-max SIZE] [--wait SECONDS] [--corpus <dir>] [--sessions <dir>] " +
  "[--prep N] [--no-cleanup] [--force-recut] [--strict-census] [--strict]"

/** The roots a line states; an absent one is the deployment's. */
export const overridesOf = (input: PathInput): Overrides => ({
  corpus: Option.getOrUndefined(input.corpus),
  sessions: Option.getOrUndefined(input.sessions),
  registry: Option.getOrUndefined(input.registry),
  repoRoot: Option.getOrUndefined(input.repoRoot)
})

/** The roots as stated, the deployment's filling the rest. */
export const pathsOf = (overrides: Overrides) => Effect.flatMap(Roots.Service, (roots) => roots.paths(overrides))

/** The universe the switches name; the widest one stated wins. */
const universeOf = (input: TriageInput): Selection["universe"] =>
  input.standing ? "standing" : input.all ? "all" : input.known ? "known" : "named"

/** The lane-defaulted knobs as the line states them; an absent one is unstated. */
export const statedKnobs = (input: {
  readonly concurrency: Option.Option<number>
  readonly batch: Option.Option<number>
  readonly startsPerSecond: Option.Option<number>
}): Partial<KnobResolution.Knobs> => ({
  ...Option.match(input.concurrency, { onNone: () => ({}), onSome: (concurrency) => ({ concurrency }) }),
  ...Option.match(input.batch, { onNone: () => ({}), onSome: (batchSize) => ({ batchSize }) }),
  ...Option.match(input.startsPerSecond, { onNone: () => ({}), onSome: (startsPerSecond) => ({ startsPerSecond }) })
})

/** The defaults of a line that cuts and then replays on a lane whose defaults are `lane`: the lane's pace alone. */
export const cutAndReplayDefaults = (lane: KnobResolution.Knobs): KnobResolution.Knobs => ({
  ...KnobResolution.GENERIC,
  startsPerSecond: lane.startsPerSecond
})

/** The line's options, each knob it leaves unstated taking `defaults`: the generic ones unless a caller names a lane's. */
export const optionsOf = Effect.fn("Options.of")(function*(
  input: TriageInput,
  defaults: KnobResolution.Knobs = KnobResolution.GENERIC
) {
  const knobs = KnobResolution.resolve(statedKnobs(input), undefined, defaults)
  return {
    paths: yield* pathsOf(overridesOf(input)),
    lane: input.lane,
    selection: {
      universe: universeOf(input),
      positional: input.captures,
      onlyNew: input.onlyNew,
      maxSpanSeconds: Option.getOrUndefined(input.maxSpan),
      greenOnLane: Option.getOrUndefined(input.greenOn),
      sampleSize: Option.getOrUndefined(input.sample),
      limit: Option.getOrUndefined(input.limit),
      cases: input.cases
    },
    batchSize: Math.max(1, knobs.batchSize),
    concurrency: Math.max(1, knobs.concurrency),
    startsPerSecond: Math.max(0, knobs.startsPerSecond),
    memoryMax: Option.getOrElse(input.memoryMax, () => DEFAULT_MEMORY_MAX),
    patience: patienceOf(input.wait),
    dryRun: input.dryRun,
    prep: Option.getOrUndefined(input.prep),
    cleanup: input.cleanup,
    forceRecut: input.forceRecut,
    strictCensus: input.strictCensus
  } satisfies Options
})

/** A line that replays on its lane: the knobs it leaves unstated take the lane's defaults. */
export const replayOptionsOf = (input: TriageInput) =>
  Effect.flatMap(KnobResolution.forLane(input.lane), (defaults) => optionsOf(input, defaults))

/** A line that cuts and then replays on its lane: {@link cutAndReplayDefaults}. */
export const cutAndReplayOptionsOf = (input: TriageInput) =>
  Effect.flatMap(KnobResolution.forLane(input.lane), (lane) => optionsOf(input, cutAndReplayDefaults(lane)))

/**
 * A request's replay: on the request's lane, each knob as the line states
 * it, else as the request carries it, else the lane's default. The selection
 * is the request's own: a universe request sweeps and tidies after itself,
 * any other is a named list and pays for no walk of the tree.
 */
export const requestOptionsOf = Effect.fn("Options.request")(function*(input: TriageInput, row: Requests.Row) {
  const requested: Partial<Requests.Knobs> = row.knobs
  const knobs = KnobResolution.resolve(statedKnobs(input), requested, yield* KnobResolution.forLane(row.lane))
  const base = yield* optionsOf(input, knobs)
  return {
    ...base,
    lane: row.lane,
    selection: {
      ...base.selection,
      universe: row.selection.kind === "universe" ? row.selection.universe : "named",
      positional: [],
      cases: []
    },
    memoryMax: Option.getOrElse(input.memoryMax, () => requested.memoryMax ?? DEFAULT_MEMORY_MAX),
    cleanup: input.cleanup && (requested.cleanup ?? DEFAULT_CLEANUP)
  } satisfies Options
})
