/**
 * A replay's session with its tools brought up to date FIRST: a build is a
 * precondition of the run, never part of the window a real-clock lane is
 * measured in. The interpreter, and the SUT where the lane manages its own,
 * are built before the lane and the box are taken; every later call in the
 * process finds them current (`Tools.ready`).
 *
 * A build that compiled leaves the box loaded for a while after it ends, and a
 * real-clock lane reads that load as a SUT that never answered. So such a lane
 * waits {@link SETTLE_MS}, counted from the moment it holds the lane and the
 * box quiet — nothing else loads the box from then on — and the run says so
 * on a `[timing]` line (`replay.settle`), so a rate taken across a build is
 * recognised rather than believed. What counts as the build is the build's
 * own time (`Tools.compileMs`): waiting for another process's build to finish
 * compiles nothing here.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { LaneHosting, Tools } from "../deployment/index.js"
import type * as Locks from "../locks.js"
import type { Options } from "../options.js"
import * as Session from "../session.js"
import * as Timing from "../timing.js"

/** How long a real-clock lane waits after a build that compiled. */
export const SETTLE_MS = 30_000

/** A build running at least this long compiled something; on a current tree it returns in a second or two. */
export const COMPILED_MS = 5_000

/** The tools `lane` replays with, brought up to date; how long they compiled for. */
export const forLane = Effect.fn("ReplayBuild.forLane")(function*(lane: string, watch: Locks.Watch) {
  const tools = yield* Tools.Service
  const hosting = yield* LaneHosting.Service
  const before = yield* tools.compileMs
  yield* tools.ready("interpreter", watch)
  if ((yield* hosting.lane(lane)).sut === "managed") yield* tools.ready("sut", watch)
  return (yield* tools.compileMs) - before
})

/** How long `lane` waits before its first cell after a build of `buildMs`. */
export const settleAfter = Effect.fn("ReplayBuild.settleAfter")(function*(lane: string, buildMs: number) {
  const state = yield* Effect.flatMap(LaneHosting.Service, (hosting) => hosting.lane(lane))
  return state.clock === "real" && buildMs >= COMPILED_MS ? SETTLE_MS : 0
})

/** What a test swaps out: the build, and the wait. */
export interface Seams {
  readonly build?: (
    lane: string,
    watch: Locks.Watch
  ) => Effect.Effect<number, Tools.ToolUnavailable | LaneHosting.LaneUnstated>
  readonly sleep?: (ms: number) => Effect.Effect<void>
}

/**
 * The tools built, then {@link Session.forReplay} held for the caller's
 * scope, then the settle a real-clock lane owes a build that compiled.
 */
export const session = (options: Options, what: string, watch: Locks.Watch = () => Effect.void, seams: Seams = {}) =>
  Effect.gen(function*() {
    const built = seams.build === undefined
      ? yield* forLane(options.lane, watch)
      : yield* seams.build(options.lane, watch)
    yield* Session.forReplay(options, what, watch)
    const settle = yield* settleAfter(options.lane, built)
    if (settle === 0) return
    yield* Console.log(
      `[replay] the build took ${(built / 1000).toFixed(1)} s: settling ${settle / 1000} s ` +
        `before the first cell on ${options.lane}`
    )
    Timing.emit({ phase: "replay.settle", ms: settle, steps: { build: built } })
    yield* (seams.sleep ?? ((ms: number) => Effect.sleep(ms)))(settle)
  })
