/**
 * A cell's `cell.v1` built from its bundle carries every Call-ID the test side
 * sent or received, the base keys read off its legs, and its timeline on the
 * lane's clock and on the wall clock. The fixture `simulated-cell` is a bundle
 * this repository's interpreter wrote on a paused clock (the linear-attempt
 * rung of `pivot-interpreter`'s run ladder): its clocks part, 1222 ms of run
 * against 22 ms of wall.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CellOfBundle from "../src/cell-of-bundle.js"
import { run } from "./tree.js"

const SIMULATED = path.join(import.meta.dirname, "fixtures/simulated-cell")

const PLACEMENT: CellOfBundle.Placement = {
  id: "linear-attempt__demo",
  case: "linear-attempt",
  lane: "upstream-demo",
  batch: "b1",
  attempt: 1,
  claims: { "call-records": { count: 1 } },
  keys: { call_id: ["A-18e703c5c4a9e6ac0@pivot.invalid"], account: ["0009001"] }
}

const made: Array<string> = []
afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** The fixture copied, `edit` applied to its files by name. */
const variant = (edit: (files: Record<string, string>) => void): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cell-of-bundle-"))
  made.push(dir)
  fs.cpSync(SIMULATED, dir, { recursive: true })
  const files: Record<string, string> = {}
  for (const name of ["run-config.json", "timing.json"]) files[name] = fs.readFileSync(path.join(dir, name), "utf8")
  edit(files)
  for (const [name, text] of Object.entries(files)) {
    if (text === "") fs.rmSync(path.join(dir, name))
    else fs.writeFileSync(path.join(dir, name), text)
  }
  return dir
}

const describeDir = async (dir: string) => {
  const result = await run(CellOfBundle.ofBundle(dir, PLACEMENT))
  if (Result.isFailure(result)) throw new Error(result.failure.message)
  return result.success
}

describe("cell.v1 of a bundle", () => {
  it("carries every Call-ID the test side sent or received, the declared keys merged in", async () => {
    const cell = await describeDir(SIMULATED)
    expect(cell.keys).toEqual({
      call_id: ["A-18e703c5c4a9e6ac0@pivot.invalid", "1-A-18e703c5c4a9e6ac0@pivot.invalid"],
      caller: ["0009001"],
      callee: ["bob5070"],
      request_uri_user: ["bob5070"],
      account: ["0009001"]
    })
  })

  it("states a simulated cell's timeline on both clocks, apart", async () => {
    const cell = await describeDir(SIMULATED)
    expect(cell.clock).toBe("simulated")
    expect(cell.timeline).toEqual({
      start: { lane_ms: 0, wall_ms: 1794407124473 },
      end: { lane_ms: 1222, wall_ms: 1794407124495 }
    })
  })

  it("describes each leg: its actor's kind, Call-ID, From, To, request-URI and instants", async () => {
    const cell = await describeDir(SIMULATED)
    expect(cell.legs).toEqual([
      {
        id: "A",
        role: "uac",
        actor: "uac1",
        call_id: "A-18e703c5c4a9e6ac0@pivot.invalid",
        from: "sip:0009001@pivot.invalid",
        to: "sip:bob5070@pivot.invalid",
        request_uri: "sip:bob5070@127.0.0.1:5080",
        start_us: 0,
        answer_us: 652000,
        end_us: 1102000
      },
      {
        id: "B",
        role: "uas",
        actor: "uas1",
        call_id: "1-A-18e703c5c4a9e6ac0@pivot.invalid",
        from: "sip:0009001@pivot.invalid",
        to: "sip:bob5070@pivot.invalid",
        request_uri: "sip:bob5070@127.0.0.1:5070",
        start_us: 202000,
        answer_us: 452000,
        end_us: 1102000
      }
    ])
  })

  it("keeps what the base placed: identity, claims, directory", async () => {
    const cell = await describeDir(SIMULATED)
    expect([cell.schema, cell.id, cell.case, cell.lane, cell.batch, cell.attempt, cell.claims, cell.dir]).toEqual([
      "cell.v1",
      "linear-attempt__demo",
      "linear-attempt",
      "upstream-demo",
      "b1",
      1,
      { "call-records": { count: 1 } },
      SIMULATED
    ])
  })

  it("reads a real-clock cell's wall clock from its sut-join.json where timing.json states none", async () => {
    const dir = variant((files) => {
      files["run-config.json"] = files["run-config.json"]!.replace(`"clock": "virtual"`, `"clock": "real"`)
      files["timing.json"] = `{"settle_budget_ms": 10000, "settled_at_ms": 1222, "started_at_ms": 0}\n`
    })
    fs.writeFileSync(
      path.join(dir, "sut-join.json"),
      JSON.stringify({ call_ids: ["A-18e703c5c4a9e6ac0@pivot.invalid"], started_at_ms: 1700000000000, ended_at_ms: 1700000001223 })
    )
    const cell = await describeDir(dir)
    expect([cell.clock, cell.timeline]).toEqual([
      "real",
      { start: { lane_ms: 0, wall_ms: 1700000000000 }, end: { lane_ms: 1222, wall_ms: 1700000001223 } }
    ])
  })

  it("refuses a bundle that states no wall clock", async () => {
    const dir = variant((files) => {
      files["timing.json"] = `{"settle_budget_ms": 10000, "settled_at_ms": 1222, "started_at_ms": 0}\n`
    })
    const result = await run(CellOfBundle.ofBundle(dir, PLACEMENT))
    expect(Result.isFailure(result) && result.failure.reason).toBe("neither timing.json nor sut-join.json states the wall clock")
  })

  it("reads a URI's user, a telephone-subscriber's number without its parameters", () => {
    expect([
      CellOfBundle.userOf("sip:+15550100;ext=7@example.invalid;user=phone"),
      CellOfBundle.userOf("sip:alice;day=tuesday@example.invalid"),
      CellOfBundle.userOf("tel:+15550100;phone-context=example.invalid"),
      CellOfBundle.userOf("sip:example.invalid")
    ]).toEqual(["+15550100", "alice;day=tuesday", "+15550100", undefined])
  })

  it("ends an unsettled run's lane clock at its last datagram", async () => {
    const dir = variant((files) => {
      files["timing.json"] = `{"settle_budget_ms": 10000, "started_at_ms": 0, "wall_ended_at_ms": 1700000000900, "wall_started_at_ms": 1700000000000}\n`
    })
    expect((await describeDir(dir)).timeline.end).toEqual({ lane_ms: 1102, wall_ms: 1700000000900 })
  })
})
