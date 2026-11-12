/**
 * A cell judged off its bundle: a failed verdict's checks are the pack's
 * typer's, its message the verdict's failures; a passing cell never asks the
 * typer; an error is the tool's summary.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import type { Cell } from "../src/cell.js"
import { judge, type Typer } from "../src/judge.js"
import type * as Results from "@sip/store/campaign-results"

const cell = (dir: string): Cell => ({
  id: "c",
  ref: "Flows/c",
  subcase: "s",
  retries: 0,
  tags: [],
  alone: false,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  checked: [],
  source: { file: "cases/c.ts", text: "" },
  locks: [],
  needs: [],
  dir,
  endpoints: ["alice", "bob"],
  egress: "bob"
})

/** A bundle directory holding `verdict`, and the typer's calls recorded with the check it answers. */
const bundled = (verdict: unknown) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "runner-judge-"))
  fs.writeFileSync(path.join(dir, "verdict.json"), JSON.stringify(verdict))
  const asked: Array<number> = []
  const typed: Results.Check = { anchor: "B1 INVITE", field: "ruri.user", expected: "a", seen: "b", said: null, side: "sut" }
  const typer: Typer = (failed) => Effect.sync(() => (asked.push(failed.failures.length), [typed]))
  return { dir, asked, typed, typer }
}

const judged = (typer: Typer, dir: string, exit: number) =>
  Effect.runPromise(judge(typer, "hosted", cell(dir), exit, "the tool said so", dir).pipe(Effect.provide(NodeServices.layer)))

describe("a judged cell", () => {
  it("is a failure whose checks the pack's typer states, its message each verdict failure", async () => {
    const b = bundled({ status: "failed", failures: [{ failure: "check-failed", site: 'step "s1"', field: "ruri.user", op: "eq", expected: "a", observed: "b" }] })
    const result = await judged(b.typer, b.dir, 1)
    expect(result.status).toBe("failure")
    expect(result.checks).toEqual([b.typed])
    expect(result.message).toBe('step "s1": ruri.user eq "a", observed "b"')
    expect(b.asked).toEqual([1])
  })

  it("is a pass the typer is never asked of", async () => {
    const b = bundled({ status: "passed" })
    const result = await judged(b.typer, b.dir, 0)
    expect([result.status, result.checks]).toEqual(["pass", []])
    expect(b.asked).toEqual([])
  })

  it("is an error stating the tool's summary where no call was judged", async () => {
    const b = bundled({ status: "passed" })
    const result = await judged(b.typer, b.dir, 3)
    expect([result.status, result.message]).toEqual(["error", "the tool said so"])
    expect(b.asked).toEqual([])
  })
})
