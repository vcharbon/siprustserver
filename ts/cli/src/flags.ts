/**
 * The FLAG SETS the commands share, as `effect/unstable/cli` definitions: the
 * roots a line may state, the lane, the patience for a held lane, and the
 * triage words `cut`, `replay` and `evidence` take alike — the same words for
 * the same things, so a two-command session is the one-command session with a
 * cut in front of it.
 */
import {
  DEFAULT_BATCH,
  DEFAULT_CLEANUP,
  DEFAULT_CONCURRENCY,
  DEFAULT_MEMORY_MAX,
  DEFAULT_STARTS_PER_SECOND
} from "@sip/corpus/knobs"
import * as Locks from "@sip/corpus/locks"
import * as Option from "effect/Option"
import { Argument, type Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "./facts.js"
import * as IdFlags from "./id-flags.js"

/** Where the roots and the registry are; absent means the deployment's (`Roots`). */
export const pathFlags = {
  corpus: Flag.string("corpus").pipe(
    Flag.withDescription("The raw corpus: one directory per capture"),
    Flag.optional
  ),
  sessions: Flag.string("sessions").pipe(
    Flag.withDescription("The session tree: run bundles, cut logs, content cache"),
    Flag.optional
  ),
  registry: Flag.string("registry").pipe(
    Flag.withDescription("The verdict registry file"),
    Flag.optional
  ),
  repoRoot: Flag.string("repo-root").pipe(
    Flag.withDescription("The repository root the registry's tickets resolve under"),
    Flag.optional
  )
}

export type PathInput = Command.Command.Config.Infer<typeof pathFlags>

/** The lane to drive, the deployment's default where the line names none. */
export const laneFlag = (facts: CliFacts) =>
  Flag.string("lane").pipe(
    Flag.withDescription("The lane to drive"),
    Flag.withDefault(facts.defaultLane)
  )

/** Seconds to queue for a held lane or box; absent is the locks' own patience. */
export const waitFlag = Flag.float("wait").pipe(
  Flag.withDescription("Seconds to wait for a held lane or box"),
  Flag.optional
)

export const patienceOf = (wait: Option.Option<number>): Locks.Patience => ({
  waitMs: Option.match(wait, {
    onNone: () => Locks.DEFAULT_WAIT_MS,
    onSome: (seconds) => seconds * 1000
  })
})

/** The command line the triage phases share: which captures, which lane, what shape, how long to wait. */
export const triageFlags = (facts: CliFacts) => ({
  ...pathFlags,
  captures: Argument.string("capture").pipe(
    Argument.withDescription("A pcap file name, extension and all"),
    Argument.variadic()
  ),
  known: Flag.boolean("known").pipe(
    Flag.withDescription("The queue: pending registry records and current failures"),
    Flag.withDefault(false)
  ),
  all: Flag.boolean("all").pipe(
    Flag.withDescription("The whole universe: the standing cut and every recorded capture"),
    Flag.withDefault(false)
  ),
  standing: Flag.boolean("standing").pipe(
    Flag.withDescription("The standing cut alone"),
    Flag.withDefault(false)
  ),
  onlyNew: Flag.boolean("only-new").pipe(
    Flag.withDescription("Only captures the lane has never run"),
    Flag.withDefault(false)
  ),
  maxSpan: Flag.float("max-span").pipe(
    Flag.withDescription(
      "Only the calls spanning at most this many seconds; a capture with calls over it replays as a cases set"
    ),
    Flag.optional
  ),
  greenOn: Flag.string("green-on").pipe(
    Flag.withDescription("Only captures whose latest run on this lane was green"),
    Flag.optional
  ),
  sample: Flag.integer("sample").pipe(
    Flag.withDescription("A stable sample of this many captures"),
    Flag.optional
  ),
  limit: Flag.integer("limit").pipe(
    Flag.withDescription("At most this many captures, in capture order"),
    Flag.optional
  ),
  cases: Flag.string("case").pipe(
    Flag.withDescription(
      "replay only this case (repeatable; caseN of the named capture, a full case id, runN/caseN, a Call-ID); a cases run"
    ),
    Flag.atLeast(0)
  ),
  dryRun: Flag.boolean("dry-run").pipe(
    Flag.withDescription("Print the work list and do nothing"),
    Flag.withDefault(false)
  ),
  lane: laneFlag(facts),
  batch: Flag.integer("batch").pipe(
    Flag.withDescription("Captures per batch"),
    Flag.withDefault(DEFAULT_BATCH)
  ),
  concurrency: Flag.integer("concurrency").pipe(
    Flag.withDescription("Cells at once"),
    Flag.withDefault(DEFAULT_CONCURRENCY)
  ),
  startsPerSecond: Flag.float("starts-per-second").pipe(
    Flag.withDescription("Cell starts per second (0: as fast as slots free)"),
    Flag.withDefault(DEFAULT_STARTS_PER_SECOND)
  ),
  memoryMax: Flag.string("memory-max").pipe(
    Flag.withDescription("The memory scope the cut and the driver run under"),
    Flag.withDefault(DEFAULT_MEMORY_MAX)
  ),
  wait: waitFlag,
  slice: Flag.integer("slice").pipe(
    Flag.withDescription("cut: captures per slice of a corpus-wide re-cut"),
    Flag.optional
  ),
  resume: IdFlags.flag("resume", "prep").pipe(
    Flag.withDescription("cut: the prep a sliced re-cut continues"),
    Flag.optional
  ),
  prep: IdFlags.flag("prep", "prep").pipe(
    Flag.withDescription("replay: the prep every capture is read as of"),
    Flag.optional
  ),
  cleanup: Flag.boolean("cleanup").pipe(
    Flag.withDescription("Run the retention pass after a sweep; --no-cleanup keeps the tree as the sweep left it"),
    Flag.withDefault(DEFAULT_CLEANUP)
  ),
  forceRecut: Flag.boolean("force-recut").pipe(
    Flag.withDescription("evidence: cut the named captures again although their standing cut is current"),
    Flag.withDefault(false)
  ),
  strict: Flag.boolean("strict").pipe(
    Flag.withDescription("--known: exit 1 where a queued capture errored (the CI meaning); 0 with the summary without"),
    Flag.withDefault(false)
  ),
  strictCensus: Flag.boolean("strict-census").pipe(
    Flag.withDescription("cut: refuse the cut when the census report does not cover every capture"),
    Flag.withDefault(false)
  )
})

export type TriageInput = Command.Command.Config.Infer<ReturnType<typeof triageFlags>>
