/**
 * The held view of an async request: what is on screen while a new answer is
 * in flight. An `ok` replaces the value; a `loading` or an `error` that follows
 * keeps the last value on screen and says so beside it, so a page never
 * unmounts what it was showing to say "loading".
 */
import { describe, expect, it } from "vitest"
import type { Async } from "../src/api.ts"
import { hold, INITIAL, type Held } from "../src/held.ts"

const ok = (value: number): Async<number> => ({ state: "ok", value })
const loading: Async<number> = { state: "loading" }
const error = (message: string): Async<number> => ({ state: "error", message })

describe("hold", () => {
  it("takes an ok answer as the current value, not loading, with no error", () => {
    const held: Held<number> = hold(INITIAL, ok(1))
    expect(held).toEqual({ current: 1, loading: false })
  })

  it("keeps the last value on screen while the next answer loads", () => {
    const held = hold(hold(INITIAL, ok(1)), loading)
    expect(held).toEqual({ current: 1, loading: true })
  })

  it("keeps the last value beside the error the next answer failed with", () => {
    const held = hold(hold(INITIAL, ok(1)), error("boom"))
    expect(held).toEqual({ current: 1, loading: false, error: "boom" })
  })

  it("replaces the value and drops the error once a later answer is ok", () => {
    const held = hold(hold(hold(INITIAL, ok(1)), error("boom")), ok(2))
    expect(held).toEqual({ current: 2, loading: false })
  })

  it("carries an error with nothing held as an error over nothing", () => {
    expect(hold(INITIAL, error("boom"))).toEqual({ current: undefined, loading: false, error: "boom" })
  })
})
