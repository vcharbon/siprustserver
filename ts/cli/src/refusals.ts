/**
 * The REFUSALS a command asks before it starts work that would fail a hundred
 * captures deep: the lane named, the census run a cut declares from, and the
 * host's ability to box the cut's child (`Boxing.refusal`). Each is a line
 * naming the fix, or `null`.
 */
import { Boxing, Census, LaneHosting } from "@sip/corpus"
import type { Paths } from "@sip/corpus/paths"
import * as Effect from "effect/Effect"
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

/** Why a cut cannot run here, or `null`: the census it reads, then the host's box for its child. */
export const cut = (facts: CliFacts, paths: Paths) =>
  Effect.gen(function*() {
    const refused = yield* census(facts, paths)
    if (refused !== null) return refused
    return yield* Effect.flatMap(Boxing.Service, (boxing) => boxing.refusal)
  })
