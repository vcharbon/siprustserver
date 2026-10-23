/**
 * Closing a prep whose cut is gone: the prep ended on the evidence a sweep's
 * close asks for — no live job, no reading recorded lately — and refused,
 * closing nothing, where either is missing.
 */
import { Jobs, OpenPreps } from "@sip/store"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import { close } from "../../src/housekeeping/prep-close.js"
import { QUIET_MINUTES } from "../../src/housekeeping/sweep-close.js"

const NOW = Date.UTC(2026, 9, 12, 8, 0, 0)
const PREP = 20

const store = (prep: Partial<OpenPreps.OpenPrep> | null, job?: Jobs.Row) => {
  const closed: Array<number> = []
  const layer = Layer.mergeAll(
    Layer.succeed(OpenPreps.Service, OpenPreps.Service.of({
      all: Effect.succeed(prep === null ? [] : [{
        prepId: PREP,
        startedAt: "2026-09-23T07:37:06.000Z",
        jobId: null,
        lastCutAt: null,
        ...prep
      }]),
      close: (id) => Effect.sync(() => (closed.push(id), true))
    })),
    Layer.succeed(Jobs.Service, {
      running: Effect.succeed(job === undefined ? [] : [job])
    } as unknown as Jobs.Interface)
  )
  return { layer, closed }
}

const run = (layer: Layer.Layer<OpenPreps.Service | Jobs.Service>) =>
  Effect.runPromise(close(PREP, NOW).pipe(Effect.provide(Layer.merge(layer, NodeServices.layer))))

describe("closing a prep whose cut is gone", () => {
  it("ends a prep no job holds and that recorded nothing lately", async () => {
    const { closed, layer } = store({ lastCutAt: new Date(NOW - (QUIET_MINUTES + 5) * 60_000).toISOString() })
    expect(await run(layer)).toEqual({ _tag: "closed" })
    expect(closed).toEqual([PREP])
  })

  it("refuses, closing nothing, while it recorded a reading lately", async () => {
    const { closed, layer } = store({ lastCutAt: new Date(NOW - 60_000).toISOString() })
    expect((await run(layer))._tag).toBe("refused")
    expect(closed).toEqual([])
  })

  it("refuses while its job's process is alive", async () => {
    const job = { id: 3, pid: process.pid, logPath: null, status: "running" } as unknown as Jobs.Row
    const { closed, layer } = store({ jobId: 3 }, job)
    expect(await run(layer)).toMatchObject({ _tag: "refused" })
    expect(closed).toEqual([])
  })

  it("refuses a prep that is not open", async () => {
    expect((await run(store(null).layer))._tag).toBe("refused")
  })
})
