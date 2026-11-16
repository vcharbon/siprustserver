/**
 * The lane lists' load-time refusals, the `extends` chain (nearest wins), a
 * list standing beside the shipped ones, and the class a classification takes
 * from them.
 */
import { NodeFileSystem } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { classify, loadLanePolicy, type ListSource } from "../src/lane-lists.js"
import { section } from "../src/rule.js"
import { headerProbe } from "./probes.js"

const KNOWN: ReadonlySet<string> = new Set(["via-stack-owned", "call-id-stack-owned"])

const run = <A, E>(effect: Effect.Effect<A, E, FileSystem.FileSystem>): Promise<A> =>
  Effect.runPromise(Effect.provide(effect, NodeFileSystem.layer))

const tempLists = (files: Record<string, unknown>, suffix = ".json"): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lane-lists-"))
  for (const [name, body] of Object.entries(files)) {
    fs.writeFileSync(path.join(dir, `${name}${suffix}`), typeof body === "string" ? body : JSON.stringify(body))
  }
  return dir
}

const source = (dir: string, beside?: string): ListSource => ({ dir, beside, known: KNOWN })

/** What a refused load says: the corruption's reason, or the other error's own tag. */
const failsToLoad = async (lane: string, dir: string): Promise<string> => {
  const error = await run(Effect.flip(loadLanePolicy(lane, source(dir))))
  return error._tag === "DeltaRules.LaneListCorrupt" ? error.reason : error._tag
}

const entry = { rule: "via-stack-owned" }

describe("the loader refusals", () => {
  it("a lane with no list file is missing, not corrupt", async () => {
    expect(await failsToLoad("nowhere", tempLists({}))).toBe("DeltaRules.LaneListMissing")
  })

  it("a list that is not JSON is corrupt", async () => {
    expect(await failsToLoad("solo", tempLists({ solo: "{ not json" }))).toMatch(/^not JSON/)
  })

  it("an unknown rule id refuses", async () => {
    const dir = tempLists({ solo: { lane: "solo", accepted: [{ rule: "no-such-rule" }], known_bug: [] } })
    expect(await failsToLoad("solo", dir)).toMatch(/registry does not define/)
  })

  it("a known_bug without a ticket refuses", async () => {
    const dir = tempLists({ solo: { lane: "solo", accepted: [], known_bug: [entry] } })
    expect(await failsToLoad("solo", dir)).toMatch(/must name the ticket/)
  })

  it("an accepted entry with a ticket refuses", async () => {
    const dir = tempLists({ solo: { lane: "solo", accepted: [{ ...entry, ticket: "t" }], known_bug: [] } })
    expect(await failsToLoad("solo", dir)).toMatch(/waits on nothing/)
  })

  it("one rule in both classes of one file refuses", async () => {
    const dir = tempLists({ solo: { lane: "solo", accepted: [entry], known_bug: [{ ...entry, ticket: "t" }] } })
    expect(await failsToLoad("solo", dir)).toMatch(/both classes/)
  })

  it("an extends cycle refuses", async () => {
    const dir = tempLists({
      a: { lane: "a", extends: "b", accepted: [], known_bug: [] },
      b: { lane: "b", extends: "a", accepted: [], known_bug: [] }
    })
    expect(await failsToLoad("a", dir)).toMatch(/cycle/)
  })

  it("an entry the schema does not state refuses", async () => {
    const dir = tempLists({ solo: { lane: "solo", accepted: [], known_bug: [], note: "x" } })
    expect(await failsToLoad("solo", dir)).toMatch(/note/)
  })
})

describe("the extends chain", () => {
  it("a nearer lane overrides an inherited class", async () => {
    const dir = tempLists({
      base: { lane: "base", accepted: [entry], known_bug: [] },
      leaf: { lane: "leaf", extends: "base", accepted: [], known_bug: [{ ...entry, ticket: "T-1" }] }
    })
    const leaf = await run(loadLanePolicy("leaf", source(dir)))
    expect(leaf.knownBug.get("via-stack-owned")?.ticket).toBe("T-1")
    expect(leaf.accepted.has("via-stack-owned")).toBe(false)
  })

  it("a list beside the shipped ones stands in for its name, its chain reaching the shipped lists", async () => {
    const dir = tempLists({ base: { lane: "base", accepted: [entry, { rule: "call-id-stack-owned" }], known_bug: [] } })
    const beside = tempLists(
      { lab: { lane: "lab", extends: "base", accepted: [], known_bug: [{ ...entry, ticket: "T-2" }] } },
      ".deltas.json"
    )
    const lab = await run(loadLanePolicy("lab", source(dir, beside)))
    expect(lab.knownBug.has("via-stack-owned")).toBe(true)
    expect([...lab.accepted.keys()]).toEqual(["call-id-stack-owned"])
    const base = await run(loadLanePolicy("base", source(dir, beside)))
    expect([...base.accepted.keys()]).toEqual(["via-stack-owned", "call-id-stack-owned"])
  })
})

describe("classification", () => {
  const rules = section<unknown, "only">("only", [
    { id: "via-stack-owned", on: { kind: "header", family: ["via"] }, why: "", header: () => true },
    { id: "call-id-stack-owned", on: { kind: "header", family: ["call-id"] }, why: "", header: () => true }
  ])
  const policy = {
    lane: "lab",
    accepted: new Map([["via-stack-owned", entry]]),
    knownBug: new Map([["call-id-stack-owned", { rule: "call-id-stack-owned", ticket: "T-3" }]])
  }
  const probe = (name: string) => headerProbe({ name, captured: ["a"], replayed: ["b"] })

  it("names the first matching rule and takes its class from the lists", () => {
    expect(classify(policy, rules, probe("Via"), undefined)).toEqual({ class: "accepted", rule: "via-stack-owned", ticket: "" })
    expect(classify(policy, rules, probe("Call-ID"), undefined)).toEqual({
      class: "known-bug",
      rule: "call-id-stack-owned",
      ticket: "T-3"
    })
  })

  it("is unlisted where no list holds the rule, and unknown where no rule names the probe", () => {
    expect(classify(undefined, rules, probe("Via"), undefined)).toEqual({ class: "unlisted", rule: "via-stack-owned", ticket: "" })
    expect(classify(policy, rules, probe("To"), undefined)).toEqual({ class: "unknown", rule: "", ticket: "" })
  })
})
