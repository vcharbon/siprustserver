/**
 * The REFUSALS a command asks before it starts work that would fail a hundred
 * captures deep: the lane named, the checkout the work may run from, the
 * census run a cut declares from, and the host's ability to box the cut's
 * child (`Boxing.refusal`). Each is a line naming the fix, or `null`.
 */
import { Boxing, Census, LaneHosting } from "@sip/corpus"
import type { Paths } from "@sip/corpus/paths"
import type { Selection } from "@sip/corpus/universe"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import type { CliFacts } from "./facts.js"

/** Why a cut over these roots cannot run, or `null` when a census run is there to read. */
export const census = (facts: CliFacts, paths: Paths) =>
  Effect.map(Census.refusal(paths), (refused) =>
    refused !== null && refused.startsWith("no census run") && facts.censusHint !== undefined
      ? `${refused}; ${facts.censusHint}`
      : refused)

/**
 * Why `lane` cannot be driven, or `null`: a lane the deployment does not
 * state. Asked before any row, build or lock is taken for it.
 */
export const lane = (name: string) =>
  Effect.flatMap(LaneHosting.Service, (hosting) => hosting.lane(name)).pipe(
    Effect.as(null as string | null),
    Effect.catchTag("Corpus.LaneUnstated", (unstated) => Effect.succeed(unstated.message))
  )

/**
 * Why this checkout may not drive the work, or `null`: a sweep wider than
 * the captures named, and any work on a real-clock lane, run only where the
 * deployment's {@link CliFacts.environmentRefusal} lets them. `lane` is the
 * lane the work drives, `undefined` for work that drives none (a cut). A dry
 * run writes nothing and is let through anywhere. A lane the deployment does
 * not state is {@link lane}'s refusal, not this one's.
 */
export const environment = (
  facts: CliFacts,
  lane: string | undefined,
  universe: Selection["universe"],
  dryRun: boolean
) =>
  Effect.gen(function*() {
    if (dryRun) return null
    if (universe !== "named") {
      const wide = facts.environmentRefusal(`a --${universe} sweep`)
      if (wide !== null) return wide
    }
    if (lane === undefined) return null
    const stated = yield* Effect.flatMap(LaneHosting.Service, (hosting) => hosting.lane(lane)).pipe(Effect.option)
    if (Option.isSome(stated) && stated.value.clock === "real") {
      return facts.environmentRefusal(`work on the real-clock lane ${lane}`)
    }
    return null
  })

/** Why a cut cannot run here, or `null`: the census it reads, then the host's box for its child. */
export const cut = (facts: CliFacts, paths: Paths) =>
  Effect.gen(function*() {
    const refused = yield* census(facts, paths)
    if (refused !== null) return refused
    return yield* Effect.flatMap(Boxing.Service, (boxing) => boxing.refusal)
  })
