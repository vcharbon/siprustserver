/**
 * The run identity a replay records: the environment as the deployment's
 * `Identity` states it, verbatim — none where it states none — the branch its
 * checkout is on, and the load it ran under.
 */
import { NodeServices } from "@effect/platform-node"
import { RunIdentity } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Identity } from "../../src/deployment/index.js"
import * as ReplayIdentity from "../../src/replay/identity.js"

const LANE = "paused"
const load = { concurrency: 4, batchSize: 10, startsPerSecond: 0, maxSpanSeconds: null, memoryMax: "1G" }

/** A checkout git cannot read: no branch to state. */
const nowhere = fs.mkdtempSync(path.join(os.tmpdir(), "identity-test-"))
afterAll(() => fs.rmSync(nowhere, { recursive: true, force: true }))

const identityOf = (environment: string | null) =>
  Effect.runPromise(
    ReplayIdentity.current(LANE, load).pipe(
      Effect.provide(
        Layer.merge(Identity.layerWith({ environment, checkout: nowhere, repositories: [] }), NodeServices.layer)
      )
    )
  )

describe("the run identity a replay records", () => {
  it("takes the stated environment verbatim and records none where none is stated", async () => {
    expect((await identityOf("task-1")).env).toBe("task-1")
    expect((await identityOf(null)).env).toBeNull()
  })

  it("records the load and its lane hash, and no branch where git cannot say one", async () => {
    const identity = await identityOf("task-1")
    expect(identity.load).toEqual(load)
    expect(identity.laneHash).toBe(RunIdentity.laneHash(LANE, load))
    expect(identity.branch).toBeNull()
  })
})
