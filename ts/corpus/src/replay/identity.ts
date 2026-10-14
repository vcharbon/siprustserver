/**
 * Who makes a run and under what load, as this process reads it: the
 * environment the deployment's `Identity` states, verbatim — a host that
 * states none records none, and its runs are never the current population's
 * — the branch its checkout is on, and the load the replay applies on the
 * lane.
 *
 * The branch is read once per process: a sweep's thousands of runs share one
 * checkout.
 */
import { RunIdentity } from "@sip/store"
import { Git } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { Identity } from "../deployment/index.js"

const branches = new Map<string, string | null>()

const branchOf = (checkout: string) =>
  Effect.suspend(() =>
    branches.has(checkout)
      ? Effect.succeed(branches.get(checkout)!)
      : Effect.tap(Git.branchOf(checkout), (branch) => Effect.sync(() => void branches.set(checkout, branch)))
  )

/** The run identity of a replay on `lane` under `load`. */
export const current = (
  lane: string,
  load: RunIdentity.Load
): Effect.Effect<RunIdentity.RunIdentity, never, Identity.Service | ChildProcessSpawner.ChildProcessSpawner> =>
  Effect.gen(function*() {
    const identity = yield* Identity.Service
    return {
      env: identity.environment,
      branch: yield* branchOf(identity.checkout),
      laneHash: RunIdentity.laneHash(lane, load),
      load
    }
  })
