/**
 * `campaign` — the operator's one command: pcaps in, runs on a lane out.
 *
 *   campaign --pcaps <dir-or-file>... [--set <file>...] [--lane L]
 *       [--add-only | --add-and-replay]
 *       [--max-span S] [--sample N] [--limit N] [--concurrency N] [--batch N]
 *       [--memory-max SIZE] [--prep N] [--no-cleanup] [--force-recut] [--dry-run]
 *       [--repo DIR] [--wait SECONDS]
 *
 * `--pcaps` names captures one file each; `--set` names the consecutive files
 * of ONE capture (a call may start in one and end in the next), which the
 * extractor refuses when they are not one tap's ring. No `--lane` is add-only:
 * the captures are extracted, added to the population and cut, and nothing is
 * driven. `--lane` drives that lane afterwards (`@sip/corpus` `campaign.ts`).
 *
 * The host's env files were applied by the runner before the layers were
 * built, so a store url a file states is the one used; the campaign opens by
 * saying what each file set (`HostEnv.Applied`).
 */
import * as Campaign from "@sip/corpus/campaign"
import * as Population from "@sip/corpus/population"
import { Boxing } from "@sip/corpus"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Command } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import * as HostEnv from "../host-env.js"
import * as Refusals from "../refusals.js"
import { exit, operatorCwd } from "../runner.js"
import { flags, type Input, optionsOf, usage } from "./campaign-args.js"

/** Where the pcaps come from: what was stated, else the host's default — unless a `--set` alone was named. */
const pcapsOf = (facts: CliFacts, input: Input): ReadonlyArray<string> =>
  input.pcaps.length > 0 || input.set.length > 0 ? input.pcaps : facts.defaultPcaps()

const refuse = (why: string) => Console.error(`[campaign] refused: ${why}`).pipe(Effect.as(2))

const main = Effect.fn("Campaign.main")(function*(facts: CliFacts, input: Input) {
  const lane = Option.getOrUndefined(input.lane)
  if (input.addOnly && input.addAndReplay) return yield* refuse("--add-only and --add-and-replay say opposite things")
  if (input.addAndReplay && lane === undefined) return yield* refuse("--add-and-replay needs the --lane to replay on")
  // A dry run reads the shared store and writes nothing, so any checkout may
  // ask it what the deployment's own checkout would do.
  const environment = input.dryRun ? null : facts.environmentRefusal("the campaign")
  if (environment !== null) return yield* refuse(environment)
  const pcaps = pcapsOf(facts, input)
  if (pcaps.length === 0 && input.set.length === 0) {
    return yield* refuse(`no --pcaps, no --set and no default pcaps for this host\n${usage(facts)}`)
  }
  const options = yield* optionsOf(facts, input)
  const refused = yield* Population.refusal(options.paths, options.selection)
  if (refused !== null) return yield* refuse(refused)
  const census = yield* Refusals.census(facts, options.paths)
  if (census !== null) return yield* refuse(census)
  // Asked before anything spawns: every long child of this pipeline runs boxed,
  // and a dry run spawns none.
  const boxed = options.dryRun ? null : yield* Effect.flatMap(Boxing.Service, (boxing) => boxing.refusal)
  if (boxed !== null) return yield* refuse(boxed)
  const applied = yield* HostEnv.Applied
  return yield* Campaign.run({
    options,
    pcaps,
    set: input.set,
    cwd: operatorCwd(),
    lane: input.addOnly ? undefined : lane,
    forceRecut: input.forceRecut,
    opening: applied.map((one) =>
      `env    ${one.file}: ${one.set.length === 0 ? "nothing to set" : one.set.join(", ")}`
    ),
    where: facts.where
  })
})

export const command = (facts: CliFacts) =>
  Command.make("campaign", flags(facts), (input) => main(facts, input).pipe(Effect.flatMap(exit))).pipe(
    Command.withDescription("Pcaps to runs: ingest, add to the population, cut, replay")
  )
