/**
 * WHICH lock each phase of the pipeline takes.
 *
 * `locks.ts` says how mutual exclusion works; this says who needs it. A CUT
 * needs nothing: its prep number comes from a sequence and everything under
 * it is named by content, so two cuts cannot write over each other. A REPLAY
 * drives a lane, so it takes two things: the lane alone where the lane's SUT
 * is a box-wide singleton, and the box quiet where the lane's clock is real
 * (`LaneHosting`).
 *
 * The order is LANE, then BOX, and a replay queueing for a singleton SUT holds
 * neither — the box quiet stops every other replay on the host, and taking it
 * before the lane is how two sessions come to wait on each other.
 *
 * A session is an Effect RESOURCE: taken into the caller's scope and given
 * back when that scope closes, on success, failure and interruption alike.
 * The one way a lock outlives its holder is the process dying without closing
 * its scope, and for that the stale-holder rule of `locks.ts` stands.
 */
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import type * as Scope from "effect/Scope"
import { LaneHosting, type Roots } from "./deployment/index.js"
import * as Locks from "./locks.js"
import type { Options } from "./options.js"

/** The lane or the box, refused: `held` names who has it and since when. */
export class Held extends Schema.TaggedError<Held>()("Session.Held", { held: Schema.String }) {
  override get message(): string {
    return this.held
  }
}

/**
 * One lock into the scope: waited for on the fiber, or `Held`; released with
 * the scope. The WAIT is interruptible — a stop that lands while a lock is
 * queued for ends the wait at once — and a lock once taken is handed to the
 * scope before anything can interrupt again.
 */
const acquire = <E, R>(
  take: Effect.Effect<Locks.Taken | Locks.Held, E, R>
): Effect.Effect<Locks.Taken, Held | E, R | Scope.Scope> =>
  Effect.uninterruptibleMask((restore) =>
    Effect.gen(function*() {
      const taken = yield* restore(take)
      if (!Locks.isTaken(taken)) return yield* Effect.fail(new Held({ held: taken.held }))
      yield* Effect.addFinalizer(() => taken.release)
      return taken
    })
  )

/**
 * The lane where the lane is a singleton, then the box, both held for the
 * caller's scope. A box that is held gives the lane back with the failure,
 * because the scope's release runs on the way out. `watch` hears whom it
 * waits for while either is taken.
 */
export const forReplay = (
  options: Pick<Options, "lane" | "patience">,
  what: string,
  watch?: Locks.Watch
): Effect.Effect<
  void,
  Held | Roots.RootsUnstated | LaneHosting.LaneUnstated,
  Scope.Scope | FileSystem.FileSystem | Roots.Service | LaneHosting.Service
> =>
  Effect.gen(function*() {
    const lane = yield* (yield* LaneHosting.Service).lane(options.lane)
    if (lane.exclusive) yield* acquire(Locks.exclusive(`lane-${options.lane}`, what, options.patience, watch))
    yield* acquire(
      lane.clock === "real"
        ? Locks.quiet(what, options.patience, watch)
        : Locks.load(what, options.patience, watch)
    )
  })
