/** A campaign job's row as its run closes it, however the run ends. */
import * as Cause from "effect/Cause"
import * as Exit from "effect/Exit"
import { describe, expect, it } from "vitest"
import { closeOf } from "../src/job.js"

describe("a campaign job's close", () => {
  it("is done when every cell was judged, pass or fail, failed when the run errored", () => {
    expect(closeOf(Exit.succeed(0), false)).toEqual({ status: "done", exit: 0 })
    expect(closeOf(Exit.succeed(1), false)).toEqual({ status: "done", exit: 1 })
    expect(closeOf(Exit.succeed(2), false)).toEqual({ status: "failed", exit: 2 })
  })

  it("is interrupted when the run was stopped, failed when it died otherwise", () => {
    expect(closeOf(Exit.failCause(Cause.interrupt(1)), false)).toEqual({ status: "interrupted", exit: 130 })
    expect(closeOf(Exit.die(new Error("boom")), false)).toEqual({ status: "failed", exit: 2 })
  })

  it("is failed when the job holds the mark of a lab left changed, however the run ended", () => {
    expect(closeOf(Exit.succeed(0), true)).toEqual({ status: "failed", exit: 2 })
    expect(closeOf(Exit.failCause(Cause.interrupt(1)), true)).toEqual({ status: "failed", exit: 2 })
  })
})
