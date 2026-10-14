/**
 * The closing table's exit code: a capture that errored fails a named replay,
 * while `--known` — the queue of what is already failing — answers 0 with
 * its summary unless `--strict` asks for the CI meaning; a dirty or dead batch
 * fails either way, and a stopped one answers the interrupted code.
 */
import * as Effect from "effect/Effect"
import { describe, expect, it, vi } from "vitest"
import { errorsFail } from "../../src/options.js"
import { INTERRUPTED_EXIT, report, type Replay } from "../../src/replay/replay.js"

describe("errorsFail — a replay and an evidence pass read --known alike", () => {
  it("fails on errored captures for a named list, and for --known only under --strict", () => {
    expect(errorsFail("named", false)).toBe(true)
    expect(errorsFail("all", false)).toBe(true)
    expect(errorsFail("known", false)).toBe(false)
    expect(errorsFail("known", true)).toBe(true)
  })
})

const replay = (over: Partial<Replay> = {}): Replay => ({
  sweep: 1,
  job: undefined,
  batches: 1,
  interrupted: false,
  bundles: [],
  outcomes: [
    { capture: "a.pcap.gz", status: "done", detail: "" },
    { capture: "b.pcap.gz", status: "error", detail: "no cut" }
  ],
  dirty: [],
  deadHosts: [],
  ...over
})

const code = (one: Replay, options?: { readonly errorsFail?: boolean }) => Effect.runPromise(report(one, options))

describe("report", () => {
  it("fails a replay where a capture errored", async () => {
    expect(await code(replay())).toBe(1)
  })

  it("answers 0 where errored captures are the queue's own and the caller did not ask strict", async () => {
    expect(await code(replay(), { errorsFail: false })).toBe(0)
  })

  it("fails a dirty batch whatever the caller asked", async () => {
    expect(await code(replay({ dirty: ["held a call"] }), { errorsFail: false })).toBe(1)
  })

  it("answers the interrupted code for a stopped replay, whatever else it found", async () => {
    expect(await code(replay({ interrupted: true, dirty: ["held a call"] }))).toBe(INTERRUPTED_EXIT)
  })

  it("says where the viewer studies the captures, for every caller of the table", async () => {
    const said = vi.spyOn(console, "log").mockImplementation(() => undefined)
    try {
      await code(replay())
      expect(said.mock.calls.map((call) => String(call[0]))).toContain(
        "[replay] study any of them at /triage/<capture> in the flow viewer"
      )
    } finally {
      said.mockRestore()
    }
  })
})
