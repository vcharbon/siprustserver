/**
 * Closing a sweep whose process is gone: its open runs closed with the
 * reaper's detail and the sweep ended, on the two pieces of evidence the
 * reaper asks for — and refused, closing nothing, where either is missing.
 */
import { Jobs, Runs, Sweeps } from "@sip/store"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { DETAIL } from "../../src/housekeeping/reap.js"
import { close, QUIET_MINUTES } from "../../src/housekeeping/sweep-close.js"

let root = ""

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "sweep-close-"))
})

afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const NOW = Date.UTC(2026, 9, 12, 8, 0, 0)

/** A bundle last written `minutes` before {@link NOW}. */
const bundle = (name: string, minutes: number): string => {
  const dir = path.join(root, name)
  fs.mkdirSync(dir)
  const at = new Date(NOW - minutes * 60_000)
  fs.utimesSync(dir, at, at)
  return dir
}

interface Seen {
  abandoned: Array<[ReadonlyArray<number>, string]>
  finished: Array<number>
  closedJobs: Array<number>
}

const SWEEP = 7

const sweepRow = (finishedAt: string | null): Sweeps.Row => ({
  id: SWEEP,
  startedAt: "2026-10-12T06:00:00.000Z",
  finishedAt,
  lane: "paused",
  universe: "named",
  selection: null,
  captures: 2,
  concurrency: 1,
  batchSize: 1,
  memoryMax: "1G",
  runs: 2,
  code: []
})

const store = (
  open: ReadonlyArray<Runs.Abandonable>,
  options: { readonly finishedAt?: string; readonly job?: Jobs.Row } = {}
) => {
  const seen: Seen = { abandoned: [], finished: [], closedJobs: [] }
  const unused = Effect.die(new Error("closing a sweep asked the store the wrong question"))
  const layer = Layer.mergeAll(
    Layer.succeed(Sweeps.Service, Sweeps.Service.of({
      open: () => unused,
      finish: (id) => Effect.sync(() => void seen.finished.push(id)),
      list: () => Effect.succeed([sweepRow(options.finishedAt ?? null)])
    })),
    Layer.succeed(Runs.Service, {
      openOfSweep: () => Effect.succeed(open),
      abandon: (ids: ReadonlyArray<number>, detail: string) =>
        Effect.sync(() => {
          seen.abandoned.push([ids, detail])
          return ids.length
        })
    } as unknown as Runs.Interface),
    Layer.succeed(Jobs.Service, {
      running: Effect.succeed(options.job === undefined ? [] : [options.job]),
      close: (id: number) => Effect.sync(() => void seen.closedJobs.push(id))
    } as unknown as Jobs.Interface)
  )
  return { layer, seen }
}

const run = (layer: Layer.Layer<Sweeps.Service | Runs.Service | Jobs.Service>) =>
  Effect.runPromise(close(SWEEP, NOW).pipe(Effect.provide(Layer.merge(layer, NodeServices.layer))))

describe("closing a sweep whose process is gone", () => {
  it("closes its open runs with the reaper's detail and ends the sweep, once every bundle went quiet", async () => {
    const { layer, seen } = store([
      { id: 11, bundlePath: bundle("run11", QUIET_MINUTES + 5) },
      { id: 12, bundlePath: path.join(root, "run12-never-written") }
    ])
    expect(await run(layer)).toEqual({ _tag: "closed", runs: [11, 12], job: null })
    expect(seen.abandoned).toEqual([[[11, 12], DETAIL]])
    expect(seen.finished).toEqual([SWEEP])
  })

  it("refuses, closing nothing, while one of its bundles is still being written", async () => {
    const { layer, seen } = store([{ id: 11, bundlePath: bundle("run11", 1) }])
    const answer = await run(layer)
    expect(answer._tag).toBe("refused")
    expect(seen.abandoned).toEqual([])
    expect(seen.finished).toEqual([])
  })

  it("refuses a sweep that already ended", async () => {
    const { layer } = store([], { finishedAt: "2026-10-12T07:00:00.000Z" })
    expect((await run(layer))._tag).toBe("refused")
  })

  it("refuses while its job's process is alive", async () => {
    const job = { id: 3, sweepId: SWEEP, pid: process.pid, logPath: null, status: "running" } as unknown as Jobs.Row
    const { layer, seen } = store([], { job })
    const answer = await run(layer)
    expect(answer).toMatchObject({ _tag: "refused" })
    expect(seen.finished).toEqual([])
  })
})
