/**
 * A detached replay start naming a collection level: one the deployment
 * refuses (`CliFacts.levelRefusal`) is refused, exit 2, before the job's row
 * is made; one it accepts goes on to make the row.
 */
import { NodeServices } from "@effect/platform-node"
import { Identity, LaneHosting, Launcher, Roots } from "@sip/corpus"
import { Jobs, Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { Command } from "effect/unstable/cli"
import { afterEach, describe, expect, it } from "vitest"
import * as Job from "../src/commands/job.js"
import type { CliFacts } from "../src/facts.js"

const exitCode = process.exitCode

afterEach(() => {
  process.exitCode = exitCode
})

/** A deployment refusing every level but `lean`, as its job entry points would. */
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
  where: () => [],
  levelRefusal: (level) => Effect.succeed(level === "lean" ? null : `no collection level ${level}`)
}

/** The request 7 on the lane `paused`. */
const REQUEST = {
  request: {
    id: 7,
    lane: "paused",
    selection: {},
    knobs: { concurrency: 1, batchSize: 1, memoryMax: "1G", startsPerSecond: 0, cleanup: true },
    createdAt: "2026-11-12T00:00:00Z",
    createdBy: "test",
    cells: 0,
    captures: 0,
    jobs: 0
  },
  cells: []
} as unknown as Requests.Detail

/** The rows `job start` on `argv` asked the store to create, the start's row made where one was. */
const createdBy = async (argv: ReadonlyArray<string>) => {
  const created: Array<Jobs.CreateRequest> = []
  const store = Layer.mergeAll(
    Layer.succeed(Requests.Service, { show: (id: number) => Effect.succeed(id === 7 ? REQUEST : null) } as unknown as Requests.Interface),
    Layer.succeed(Jobs.Service, {
      create: (request: Jobs.CreateRequest) =>
        Effect.andThen(Effect.sync(() => void created.push(request)), Effect.die("the row is made: the start stops here"))
    } as unknown as Jobs.Interface),
    LaneHosting.layerWith({
      lanes: ["paused"],
      lane: () => Effect.succeed(LaneHosting.NEUTRAL_LANE),
      standBatch: () => Effect.succeed(undefined),
      standSut: () => Effect.succeed(undefined),
      preflight: () => Effect.succeed(null)
    }),
    Identity.layerWith({ environment: "test", checkout: "/d", repositories: [] }),
    Launcher.layer,
    Roots.layerWith({
      paths: () => Effect.succeed({ corpus: "/d", sessions: "/d/sessions", census: "/d", registry: "/d", repoRoot: "/d" }),
      pcapRoots: Effect.succeed([]),
      locks: Effect.succeed("/d/locks"),
      bench: Effect.succeed("/d/bench")
    })
  )
  await Effect.runPromise(
    Effect.exit(
      (Command.runWith(Job.command(FACTS) as Parameters<typeof Command.runWith>[0], { version: "0.0.0", renderErrors: false })(
        argv
      ) as unknown as Effect.Effect<void>).pipe(Effect.provide(store), Effect.provide(NodeServices.layer))
    )
  )
  return { created, exitCode: process.exitCode }
}

describe("a detached replay start's collection level", () => {
  it("is refused, exit 2, before the job's row where the deployment refuses it", async () => {
    const { created, exitCode } = await createdBy(["start", "replay", "--request", "7", "--collection", "bogus"])
    expect(created).toEqual([])
    expect(exitCode).toBe(2)
  })

  it("goes on to make the row where the deployment accepts it", async () => {
    const { created } = await createdBy(["start", "replay", "--request", "7", "--collection", "lean"])
    expect(created.map((request) => request.kind)).toEqual(["replay"])
  })
})
