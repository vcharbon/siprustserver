/**
 * The `replay` adapter. The exit vocabulary IS the contract, so the exit
 * vocabulary is what is pinned — one case per code, plus the one nobody should
 * ever see.
 *
 * The stub never reads its run-spec; it takes the code to exit with from the
 * path's `exit<N>` marker, so one script covers the whole vocabulary.
 */
import { ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { fixture, rig } from "./harness.js"

const STUB = { REPLAY_BIN: fixture("stub-replay.sh") }

const run = (env: Record<string, string>, spec: string) =>
  Effect.runPromiseExit(
    Effect.gen(function* () {
      const cli = yield* ReplayCli.Service
      return yield* cli.run(spec)
    }).pipe(Effect.provide(rig(ReplayCli.layer, env)))
  )

const replay = async (code: number) => {
  const exit = await run(STUB, fixture(`run-exit${code}.json`))
  if (exit._tag !== "Success") throw new Error(`the stub failed: ${JSON.stringify(exit)}`)
  return exit.value
}

describe("the exit vocabulary", () => {
  it.each([
    [0, "passed"],
    [1, "failed"],
    [2, "refused"],
    [3, "environment"],
    [4, "panicked"]
  ])("maps exit %i to %s", async (code, tag) => {
    const report = await replay(code)
    expect(report.exitCode).toBe(code)
    expect(report.outcome).toEqual({ _tag: tag })
  })

  it("names a code the vocabulary does not, rather than guessing", async () => {
    expect((await replay(9)).outcome).toEqual({ _tag: "unknown", exitCode: 9 })
  })

  it("says which outcomes left a bundle for the caller to read", () => {
    const tags = ["passed", "failed", "refused", "environment", "panicked"] as const
    expect(tags.map((tag) => ReplayCli.leftABundle({ _tag: tag }))).toEqual([true, true, false, false, true])
    expect(ReplayCli.leftABundle({ _tag: "unknown", exitCode: 9 })).toBe(false)
  })
})

describe("what the adapter collects", () => {
  it("keeps both streams, whatever the exit", async () => {
    const report = await replay(2)
    expect(report.stdout).toContain("bundle written")
    expect(report.stderr).toContain("stub stderr")
  })

  it("hands the interpreter the environment the caller provides, over its own", async () => {
    const report = await Effect.runPromise(
      Effect.gen(function* () {
        const cli = yield* ReplayCli.Service
        return yield* cli.run(fixture("run-exit0.json"))
      }).pipe(
        Effect.provideService(ReplayCli.Environment, { REPLAY_STUB_ENV: "from-the-caller" }),
        Effect.provide(rig(ReplayCli.layer, STUB))
      )
    )
    expect(report.stdout).toContain("env from-the-caller")
    expect((await replay(0)).stdout).toContain("env \n")
  })

  it("reads the run-spec schema the driver emits against", async () => {
    const text = await Effect.runPromise(
      Effect.gen(function* () {
        const cli = yield* ReplayCli.Service
        return yield* cli.schema()
      }).pipe(Effect.provide(rig(ReplayCli.layer, STUB)))
    )
    expect(JSON.parse(text)).toEqual({ title: "RunSpec" })
  })
})

describe("the binary path", () => {
  it("comes from REPLAY_BIN with no default, and refuses to guess when it is unset", async () => {
    const exit = await run({}, fixture("run-exit0.json"))
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toMatch(/REPLAY_BIN/)
  })

  it("reports a binary that is not there as unavailable, not as a verdict", async () => {
    const exit = await run({ REPLAY_BIN: fixture("no-such-binary") }, fixture("run-exit0.json"))
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toContain("Toolchain.CliUnavailable")
  })
})
