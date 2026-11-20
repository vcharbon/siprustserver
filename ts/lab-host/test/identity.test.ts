/** A run's identity is the environment the pack's variable names, the pack's checkout and the repositories it names. */
import { Identity as CorpusIdentity } from "@sip/corpus"
import * as Effect from "effect/Effect"
import { afterEach, describe, expect, it } from "vitest"
import * as Identity from "../src/identity.js"
import { PACK } from "./pack.js"

const read = () =>
  Effect.runPromise(
    Effect.provide(
      Effect.gen(function*() {
        return yield* CorpusIdentity.Service
      }),
      Identity.layer(PACK, [{ name: "tool", dir: "/tool" }])
    )
  )

describe("identity", () => {
  const before = process.env.TOOL_ENV
  afterEach(() => {
    if (before === undefined) delete process.env.TOOL_ENV
    else process.env.TOOL_ENV = before
  })

  it("records a run under the environment the pack's variable names", async () => {
    process.env.TOOL_ENV = "task-1"
    expect(await read()).toEqual({
      environment: "task-1",
      checkout: "/tool",
      repositories: [{ name: "tool", dir: "/tool" }]
    })
  })

  it("an empty or unset variable names no environment", async () => {
    process.env.TOOL_ENV = ""
    expect((await read()).environment).toBeNull()
    delete process.env.TOOL_ENV
    expect((await read()).environment).toBeNull()
  })
})
