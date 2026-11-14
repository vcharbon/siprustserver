/**
 * The leg rules a cut reads (`src/cut/leg-rules.ts`): the base's default set
 * is `X-CID` against the caller leg's `Call-ID`, as Asterisk dialplans set it,
 * and the one header a deployment cutting under it projects is `X-CID`, the
 * `Call-ID` side being the leg's own.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { DEFAULT_RULES, headersOf, legRulesOf } from "../../src/cut/leg-rules.js"

describe("the base's default leg rules", () => {
  it("join a leg whose X-CID names another leg's Call-ID to it, X-CID the one header projected", async () => {
    const rules = await Effect.runPromise(legRulesOf(DEFAULT_RULES).pipe(Effect.provide(NodeServices.layer)))
    expect(rules.map((rule) => [rule.name, rule.left.header, rule.right.header, rule.window_ms])).toEqual([
      ["asterisk-x-cid", "Call-ID", "X-CID", undefined]
    ])
    expect(headersOf(rules)).toEqual(["X-CID"])
  })
})
