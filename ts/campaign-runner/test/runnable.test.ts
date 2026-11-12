/**
 * Which lanes a campaign runs on here over a fixture pack's refusal of a
 * lane: each lane's standing, the lanes a run naming none takes, the refusal
 * of a pick naming a refused lane, and what runs instead, explained with the
 * files the lanes were read from.
 */
import { type LaneCatalog, LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import { defaultPick, explained, pickRefusal, remedyOf, standingsOf } from "../src/runnable.js"

/** A lane `id` naming the store part `store`: the lanes naming one store run together. */
const onStore = (id: string, store: string): LaneView.View => {
  const lane = LaneView.simulatedBuiltIn(id, {})
  const part = { kind: "store", name: store, file: `/rig/targets/store/${store}.toml`, value: {} }
  return { ...lane, loaded: { ...lane.loaded, parts: new Map([["store", part]]) } } as unknown as LaneView.View
}

const A = onStore("a", "one")
const B = onStore("b", "one")
const C = onStore("c", "two")

/** A rig whose one shared kind is the store. */
const RIG = { parts: new Map([["store", { reads: { shared: true } }]]) }

const CATALOG = {
  dirs: { root: "/rig", lanes: "/rig/lanes", targets: "/rig/targets" },
  rig: RIG,
  lanes: [A, B, C]
} as unknown as LaneCatalog.Catalog

/** A pack refusing lane `a` here: another lab's. */
const refusal = (lane: LaneView.View) => (lane.id === "a" ? "a is another lab's lane" : null)

const none = () => "no lab here"

describe("the lanes a campaign runs on here", () => {
  it("stand each with its group and the pack's refusal, in the catalog's order", () => {
    expect(standingsOf(refusal, CATALOG).map((one) => [one.id, one.group, one.refused])).toEqual([
      ["a", "store one", "a is another lab's lane"],
      ["b", "store one", null],
      ["c", "store two", null]
    ])
  })

  it("are, for a run naming none, the first lane the pack does not refuse and those of its group", () => {
    expect(defaultPick(refusal, CATALOG).map((lane) => lane.id)).toEqual(["b"])
    expect(defaultPick(() => null, CATALOG).map((lane) => lane.id)).toEqual(["a", "b"])
    expect(defaultPick(none, CATALOG)).toEqual([])
  })

  it("refuse a pick naming a refused lane with the pack's refusal, one of two groups, and take one of one group", () => {
    expect(pickRefusal(refusal, CATALOG, [B, A])).toBe("a is another lab's lane")
    expect(pickRefusal(refusal, CATALOG, [B, C])).toMatch(/one store part: b names one, c names two/)
    expect(pickRefusal(() => null, CATALOG, [A, B])).toBeNull()
  })

  it("say what runs instead by group, the lanes read beside it, and that nothing does where the pack refuses every lane", () => {
    expect(remedyOf(refusal, CATALOG)).toMatch(/^run b\b.*\bor c\b/)
    expect(remedyOf(none, CATALOG)).toBe("no lane of this environment runs a campaign")
    expect(explained(refusal, "a is another lab's lane", [A], CATALOG).split("\n")).toEqual([
      "a is another lab's lane",
      "  the lanes read (/rig/lanes):",
      "    a: built in, no file; store one: /rig/targets/store/one.toml",
      `  instead: ${remedyOf(refusal, CATALOG)}`
    ])
  })
})
