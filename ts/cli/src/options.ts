/**
 * What a triage command line CAME TO: the roots it states over the
 * deployment's (`Roots`), and the parsed triage flags read into the options
 * the phases share, each bound's own arithmetic applied once here rather than
 * in every phase.
 */
import { Roots } from "@sip/corpus"
import type { Options } from "@sip/corpus/options"
import type { Overrides } from "@sip/corpus/paths"
import type { Selection } from "@sip/corpus/universe"
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

export const optionsOf = Effect.fn("Options.of")(function*(input: TriageInput) {
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
    batchSize: Math.max(1, input.batch),
    concurrency: Math.max(1, input.concurrency),
    startsPerSecond: Math.max(0, input.startsPerSecond),
    memoryMax: input.memoryMax,
    patience: patienceOf(input.wait),
    dryRun: input.dryRun,
    prep: Option.getOrUndefined(input.prep),
    cleanup: input.cleanup,
    forceRecut: input.forceRecut,
    strictCensus: input.strictCensus
  } satisfies Options
})
