/**
 * A landed cell that only its pending lines kept from passing, read again
 * once its batch's end has answered them: it passes exactly where every
 * line it named is answered and none failed.
 */
import { NodeServices } from "@effect/platform-node"
import type { E2e, Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { settled } from "../src/pending.js"
import { runDir } from "./harness.js"

const dirs: Array<string> = []
afterEach(() => {
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

const landed: E2e.CellSummary = {
  cell: { case: "c", shape: "pivot-replay", infra: "l" },
  passed: false,
  dir: "c__pivot-replay__l",
  pending: ["cdr"]
}

/** `summary` settled over a cell directory holding `lines`, or no lines file where `lines` is undefined. */
const settle = async (
  lines: ReadonlyArray<Verification.Verification> | undefined,
  summary: E2e.CellSummary = landed
): Promise<E2e.CellSummary> => {
  const dir = runDir("pending")
  dirs.push(dir)
  if (lines !== undefined) fs.writeFileSync(path.join(dir, "verifications.json"), JSON.stringify(lines))
  return Effect.runPromise(settled(summary, dir).pipe(Effect.provide(NodeServices.layer)))
}

const call: Verification.Verification = { name: "call", source: "interpreter", answer: "pass" }
const cdr = (answer: Verification.Answer): Verification.Verification => ({ name: "cdr", source: "cdr oracle", answer })

describe("a cell settled after its batch's end", () => {
  it("passes once every line it waited on passes", async () => {
    const after = await settle([call, cdr("pass")])
    expect(after.passed).toBe(true)
    expect(after.pending).toBeUndefined()
  })

  it("passes on a line answered not checkable, which only the lines' status reads", async () => {
    expect((await settle([call, cdr("not-checkable")])).passed).toBe(true)
  })

  it("is not passed where the end failed a line it waited on", async () => {
    const after = await settle([call, cdr("fail")])
    expect(after.passed).toBe(false)
    expect(after.pending).toBeUndefined()
  })

  it("is not passed, still naming it, where a line it waited on is still pending", async () => {
    const after = await settle([call, cdr("pending")])
    expect(after.passed).toBe(false)
    expect(after.pending).toEqual(["cdr"])
  })

  it("is not passed where its lines are gone or no longer state a line it waited on", async () => {
    expect(await settle(undefined)).toMatchObject({ passed: false, pending: ["cdr"] })
    expect(await settle([call])).toMatchObject({ passed: false, pending: ["cdr"] })
  })

  it("reads only the lines it waited on: a failure the reclassifier restated stays its own", async () => {
    const failedCall: Verification.Verification = { ...call, answer: "fail" }
    expect((await settle([failedCall, cdr("pass")], { ...landed, restated: "rule r1" })).passed).toBe(true)
  })

  it("leaves a cell that waited on nothing as it landed", async () => {
    const { pending: _, ...failed } = landed
    expect(await settle([call, cdr("pending")], failed)).toEqual(failed)
  })
})
