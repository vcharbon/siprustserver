/**
 * The collection level a job line names reaching what its handler plans
 * with: a replay and a campaign run with `--collection <level>` read that
 * level (`@sip/corpus` `Collectors.Level`, which every plan of the job's
 * collectors reads) at their first look at the lane; with no level, the
 * workspace's default.
 */
import { NodeServices } from "@effect/platform-node"
import { Collectors, LaneHosting } from "@sip/corpus"
import * as Effect from "effect/Effect"
import { Command } from "effect/unstable/cli"
import { afterEach, describe, expect, it } from "vitest"
import * as Campaign from "../src/commands/campaign.js"
import * as Replay from "../src/commands/replay.js"
import type { CliFacts } from "../src/facts.js"

const FACTS: CliFacts = {
  defaultLane: "paused",
  hostEnv: { files: [], keys: {}, pathKeys: new Set() },
  frontDoors: "",
  environmentRefusal: () => null,
  invocation: (command) => `run ${command}`,
  viewer: () => "http://viewer",
  selectorForms: "@current",
  refusalWhy: (token) => token,
  censusHint: undefined,
  defaultPcaps: () => [],
  where: () => []
}

const exitCode = process.exitCode

afterEach(() => {
  process.exitCode = exitCode
})

/**
 * The level `command` run on `argv` plans with, as its first look at the lane
 * reads it; the lane is then refused, so the handler stops there.
 */
const levelSeenBy = async (command: unknown, argv: ReadonlyArray<string>) => {
  const seen: Array<string | undefined> = []
  const hosting = LaneHosting.layerWith({
    lanes: [],
    lane: (lane) =>
      Effect.gen(function*() {
        seen.push(yield* Collectors.Level)
        return yield* Effect.fail(new LaneHosting.LaneUnstated({ lane, known: [] }))
      }),
    standBatch: () => Effect.succeed(undefined),
    standSut: () => Effect.succeed(undefined),
    preflight: () => Effect.succeed(null)
  })
  await Effect.runPromise(
    (Command.runWith(command as Parameters<typeof Command.runWith>[0], { version: "0.0.0", renderErrors: false })(argv) as unknown as Effect.Effect<void, unknown, LaneHosting.Service>).pipe(
      Effect.provide(hosting),
      Effect.provide(NodeServices.layer),
      Effect.orDie
    ) as Effect.Effect<void>
  )
  return seen
}

describe("a job line's collection level", () => {
  it("is the one a replay names, read where the replay first looks at its lane; absent, none", async () => {
    expect(await levelSeenBy(Replay.command(FACTS), ["--collection", "deep"])).toEqual(["deep"])
    expect(await levelSeenBy(Replay.command(FACTS), [])).toEqual([undefined])
  })

  it("is the one a campaign names, read where the campaign first looks at its lane; absent, none", async () => {
    expect(await levelSeenBy(Campaign.command(FACTS), ["--lane", "paused", "--collection", "on-failure-only"])).toEqual(["on-failure-only"])
    expect(await levelSeenBy(Campaign.command(FACTS), ["--lane", "paused"])).toEqual([undefined])
  })
})
