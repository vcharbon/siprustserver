/**
 * The batch process asking an inject of the harness: its `inject` line is
 * handed to the injector the cell's run was given, and the answer goes back
 * on its stdin echoing its ask id, `injected` or `refused` with the
 * injector's words; a cell run with no injector has its inject refused; an
 * inject line that does not decode is refused naming why and fails its cell
 * as an environment failure once the cell ended. The stand-in batch process asks
 * one inject per cell and ends the cell on the answer it read, which its cell
 * line carries back as its summary.
 */
import * as Effect from "effect/Effect"
import { spawn } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { attach } from "../src/replay-batch.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "replay-batch-inject-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/**
 * A batch process asking `operation:kill` at node `i1` of each cell under the
 * ask id `a<n>`, its cell line carrying the answer it read; a cell whose spec
 * names `malformed` is asked with an action that is no string.
 */
const STAND_IN = `
const rl = require("node:readline").createInterface({ input: process.stdin })
console.log(JSON.stringify({ ready: {} }))
let spec
let asks = 0
rl.on("line", (line) => {
  if (line.startsWith("{")) {
    console.log(JSON.stringify({ cell: { spec, exit: 0, summary: line } }))
    return
  }
  spec = line
  const action = spec.includes("malformed") ? 7 : "operation:kill"
  console.log(JSON.stringify({ inject: { ask: "a" + ++asks, spec, node: "i1", action } }))
})
rl.on("close", () => console.log(JSON.stringify({ end: { dirty: null } })))
`

const standIn = async () => {
  const script = path.join(dir, "batch.cjs")
  fs.writeFileSync(script, STAND_IN)
  const child = spawn(process.execPath, [script], { stdio: ["pipe", "pipe", "pipe"] })
  const attached = attach(child, "lane-x", { endWithin: 5_000, servedLine: () => "" })
  await Effect.runPromise(attached.ready)
  return attached.host
}

describe("an inject the batch process asks", { timeout: 20_000 }, () => {
  it("is answered injected once the cell's injector did its work, with what it was asked", async () => {
    const host = await standIn()
    const asked: Array<unknown> = []
    const report = await Effect.runPromise(host.run("/cells/a.json", (ask) => Effect.sync(() => void asked.push(ask))))
    expect(asked).toEqual([{ ask: "a1", spec: "/cells/a.json", node: "i1", action: "operation:kill" }])
    expect(JSON.parse(report.stdout)).toEqual({ injected: { ask: "a1", spec: "/cells/a.json", node: "i1" } })
    await Effect.runPromise(host.end)
  })

  it("is answered refused with the injector's words where it failed", async () => {
    const host = await standIn()
    const report = await Effect.runPromise(host.run("/cells/b.json", () => Effect.fail("the worker answered no state")))
    expect(JSON.parse(report.stdout)).toEqual({
      refused: { ask: "a1", spec: "/cells/b.json", node: "i1", reason: "the worker answered no state" }
    })
    await Effect.runPromise(host.end)
  })

  it("is answered refused for a cell run with no injector", async () => {
    const host = await standIn()
    const report = await Effect.runPromise(host.run("/cells/c.json"))
    const answer = JSON.parse(report.stdout) as { refused?: { ask: string; spec: string; node: string; reason: string } }
    expect([answer.refused?.ask, answer.refused?.spec, answer.refused?.node]).toEqual(["a1", "/cells/c.json", "i1"])
    expect(answer.refused?.reason).toContain("no operation step")
    await Effect.runPromise(host.end)
  })

  it("that does not decode is refused naming why, and its cell, once its line came, is an environment failure", async () => {
    const host = await standIn()
    const asked: Array<unknown> = []
    const report = await Effect.runPromise(host.run("/cells/malformed.json", (ask) => Effect.sync(() => void asked.push(ask))))
    expect(asked, "nothing performed").toEqual([])
    expect(report.exitCode).toBe(3)
    expect(report.stderr).toContain("environment: /cells/malformed.json: the lane-x batch process wrote an inject line that does not decode")
    const refused = JSON.parse(report.stderr.split("the cell ended: ")[1]!.split("\n")[0]!) as {
      refused?: { ask: string; spec: string; node: string; reason: string }
    }
    expect([refused.refused?.ask, refused.refused?.spec, refused.refused?.node]).toEqual(["a1", "/cells/malformed.json", "i1"])
    expect(refused.refused?.reason).toContain("does not decode")
    await Effect.runPromise(host.end)
  })
})
