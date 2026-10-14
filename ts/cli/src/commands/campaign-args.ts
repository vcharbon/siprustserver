/**
 * What a CAMPAIGN command line CAME TO: the flag set the campaign parses, read
 * into the options its phases share (`@sip/corpus` `options.ts`), with each
 * bound's arithmetic applied once here rather than in every phase.
 */
import { DEFAULT_STARTS_PER_SECOND } from "@sip/corpus/knobs"
import type { Options } from "@sip/corpus/options"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { type Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { patienceOf, triageFlags } from "../flags.js"
import { pathsOf } from "../options.js"

export const usage = (facts: CliFacts): string =>
  `usage: ${facts.invocation("campaign")} --pcaps <dir-or-file>... [--set <file>...] [--lane L] ` +
  "[--add-only | --add-and-replay] [--max-span SECONDS] [--sample N] [--limit N] " +
  "[--concurrency N] [--batch N] [--memory-max SIZE] [--prep N] [--no-cleanup] [--force-recut] [--strict-census] " +
  "[--dry-run] [--repo DIR] [--wait SECONDS]"

export const flags = (facts: CliFacts) => {
  const triage = triageFlags(facts)
  return {
    pcaps: Flag.string("pcaps").pipe(
      Flag.withDescription("A directory of pcaps (recursive) or one pcap file; repeatable"),
      Flag.atLeast(0)
    ),
    set: Flag.string("set").pipe(
      Flag.withDescription(
        "A file of ONE capture split across consecutive files (or a directory of them); repeatable"
      ),
      Flag.atLeast(0)
    ),
    lane: Flag.string("lane").pipe(
      Flag.withDescription("The lane to drive; absent is add-only"),
      Flag.optional
    ),
    addOnly: Flag.boolean("add-only").pipe(
      Flag.withDescription("Ingest, add to the population and cut; drive no lane"),
      Flag.withDefault(false)
    ),
    addAndReplay: Flag.boolean("add-and-replay").pipe(
      Flag.withDescription("Ingest and then drive the named lane (the default when --lane is given)"),
      Flag.withDefault(false)
    ),
    forceRecut: Flag.boolean("force-recut").pipe(
      Flag.withDescription("Cut every named capture again, whatever the cut store says"),
      Flag.withDefault(false)
    ),
    repo: Flag.string("repo").pipe(
      Flag.withDescription("The repository the registry's tickets resolve under"),
      Flag.optional
    ),
    maxSpan: triage.maxSpan,
    sample: triage.sample,
    limit: triage.limit,
    concurrency: triage.concurrency,
    batch: triage.batch,
    memoryMax: triage.memoryMax,
    strictCensus: triage.strictCensus,
    prep: triage.prep,
    cleanup: triage.cleanup,
    dryRun: Flag.boolean("dry-run").pipe(
      Flag.withDescription("Print what each phase would do and write nothing"),
      Flag.withDefault(false)
    ),
    wait: triage.wait
  }
}

export type Input = Command.Command.Config.Infer<ReturnType<typeof flags>>

/**
 * The campaign's flags as the phases' options. The words a campaign does not
 * take — the universe switches, the pace — are the phase defaults: what this
 * command names is a set of PCAPS, and the captures it works on are the ones
 * they came to. The batch shape is the campaign's own, stated in the triage
 * words, so one `--memory-max` caps both the cut's child and the replay's.
 */
export const optionsOf = Effect.fn("CampaignArgs.optionsOf")(function*(facts: CliFacts, input: Input) {
  return {
    paths: yield* pathsOf({ repoRoot: Option.getOrUndefined(input.repo) }),
    lane: Option.getOrUndefined(input.lane) ?? facts.defaultLane,
    selection: {
      universe: "named",
      positional: [],
      onlyNew: false,
      maxSpanSeconds: Option.getOrUndefined(input.maxSpan),
      greenOnLane: undefined,
      sampleSize: Option.getOrUndefined(input.sample),
      limit: Option.getOrUndefined(input.limit),
      cases: []
    },
    batchSize: Math.max(1, input.batch),
    concurrency: Math.max(1, input.concurrency),
    startsPerSecond: DEFAULT_STARTS_PER_SECOND,
    memoryMax: input.memoryMax,
    patience: patienceOf(input.wait),
    dryRun: input.dryRun,
    prep: Option.getOrUndefined(input.prep),
    cleanup: input.cleanup,
    forceRecut: input.forceRecut,
    strictCensus: input.strictCensus
  } satisfies Options
})
