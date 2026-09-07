/**
 * The cell-start PACE: how fast a campaign is allowed to begin cells, as
 * distinct from how many it may have running.
 *
 * The two bound different things. `concurrency` bounds how much of the SUT's
 * capacity a campaign occupies at once; the pace bounds how fast it ARRIVES —
 * the call-setup rate a real backend is sized for. A campaign that starts 200
 * cells the instant a slot frees offers a burst no production system sees, and
 * against a shared standing stack that burst is what gets a run refused rather
 * than answered.
 *
 * A campaign that falls behind the grid never catches up: the pace is a CEILING
 * on the arrival rate, and a burst of owed starts fired back to back would be
 * the very thing it exists to prevent. So a due slot in the past re-anchors on
 * now. Nothing here is a queue — a start already due proceeds without sleeping.
 */
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Ref from "effect/Ref"

/** Admission to start one cell. */
export interface Pace {
  /** Returns once this cell is due to start. */
  readonly admit: Effect.Effect<void>
}

/** The pace that admits everything at once: what a campaign with no rate runs on. */
export const unpaced: Pace = { admit: Effect.void }

/**
 * A pace of `perSecond` cell starts. A rate that is not a positive, finite
 * number is no rate at all, so it yields {@link unpaced} rather than a grid
 * nothing can advance along.
 */
export const perSecond = Effect.fn("Driver.Pace.perSecond")(function* (rate: number) {
  if (!Number.isFinite(rate) || rate <= 0) return unpaced
  const period = 1000 / rate
  /** When the next start is due; `0` until the first one anchors the grid. */
  const due = yield* Ref.make(0)

  const admit = Effect.gen(function* () {
    const now = yield* Clock.currentTimeMillis
    // One atomic step per start: read the slot this cell takes and book the
    // next one in the same breath, so two concurrent starts cannot take one.
    const at = yield* Ref.modify(due, (next) => {
      const at = Math.max(next, now)
      return [at, at + period] as const
    })
    if (at > now) yield* Effect.sleep(at - now)
  })
  return { admit } satisfies Pace
})
