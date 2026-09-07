/**
 * The cell-start pace: a ceiling on the arrival rate, held on a virtual clock so
 * the assertions are about the schedule and not about how fast this box runs.
 */
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import * as TestClock from "effect/testing/TestClock"
import { describe, expect, it } from "vitest"
import { perSecond, unpaced } from "../src/pace.js"

const virtual = <A, E>(body: Effect.Effect<A, E, TestClock.TestClock>): Promise<A> =>
  Effect.runPromise(body.pipe(Effect.provide(TestClock.layer())))

/** When each of `count` admissions arrived, in millis, given `concurrency` free slots. */
const arrivals = (rate: number, count: number, concurrency: number) =>
  Effect.gen(function* () {
    const pace = yield* perSecond(rate)
    const start = yield* Clock.currentTimeMillis
    const at: Array<number> = []
    const fiber = yield* Effect.forkChild(
      Effect.forEach(
        Array.from({ length: count }, (_, i) => i),
        () =>
          Effect.gen(function* () {
            yield* pace.admit
            at.push((yield* Clock.currentTimeMillis) - start)
          }),
        { concurrency, discard: true }
      )
    )
    yield* TestClock.adjust("10 seconds")
    yield* Fiber.await(fiber)
    return at
  })

describe("the cell-start pace", () => {
  it("admits the first cell without waiting", async () => {
    expect(await virtual(arrivals(2, 1, 1))).toEqual([0])
  })

  it("holds concurrent starts to one per period", async () => {
    // Ten slots are free at once; the pace is what spaces them, not the slots.
    expect(await virtual(arrivals(4, 4, 10))).toEqual([0, 250, 500, 750])
  })

  it("never fires a catch-up burst for the time it spent behind", async () => {
    const at = await virtual(
      Effect.gen(function* () {
        const pace = yield* perSecond(10)
        const start = yield* Clock.currentTimeMillis
        yield* pace.admit
        // A cell that ran long: the grid has moved on without us.
        yield* TestClock.adjust("5 seconds")
        const seen: Array<number> = []
        for (let i = 0; i < 2; i++) {
          const fiber = yield* Effect.forkChild(pace.admit)
          yield* TestClock.adjust("1 second")
          yield* Fiber.await(fiber)
          seen.push((yield* Clock.currentTimeMillis) - start)
        }
        return seen
      })
    )
    // The five seconds owe nothing: the first is due at once, the next a full
    // period later, and neither is paid back as a burst.
    expect(at[0]).toBe(6000)
    expect(at[1]).toBe(7000)
  })

  it("is no pace at all when the rate is not a positive number", async () => {
    for (const rate of [0, -1, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(await virtual(perSecond(rate))).toBe(unpaced)
    }
  })
})
