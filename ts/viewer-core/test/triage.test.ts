import { describe, expect, it } from "vitest"
import {
  callIdFamilies,
  callIdHueClass,
  callIdShadeClass,
  extendsCallId,
  shortCallId
} from "../src/flows/families.ts"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { captureSelection, documentSut, messageCoordinate } from "../src/flows/selection.ts"
import { document, expectResponse, send } from "./documents.ts"

const msg = (tsUs: number, src: string, dst: string, method: string) => ({
  ts_us: tsUs,
  src,
  dst,
  retx: false,
  probe: 0,
  raw: `${method} sip:x SIP/2.0\r\n\r\n`,
  summary: { kind: "request", method, cseq: { seq: 1, method } }
})

/** Two ingress calls, each re-emitted by an application server under a derived Call-ID. */
const capture = (evidence: readonly unknown[]) => ({
  schema: 5,
  legs: [
    { call_id: "ingress-one", final_status: 486, msgs: [msg(1_000_000, "a:1", "b:1", "INVITE")] },
    { call_id: "SDaaa-11111111", final_status: 486, msgs: [msg(1_010_000, "b:1", "c:1", "INVITE")] },
    { call_id: "1-SDaaa-11111111", final_status: 486, msgs: [msg(1_020_000, "c:1", "b:1", "INVITE")] },
    { call_id: "ingress-two", final_status: 486, msgs: [msg(2_000_000, "a:1", "b:1", "INVITE")] },
    { call_id: "SDbbb-22222222", final_status: 486, msgs: [msg(2_010_000, "b:1", "c:1", "INVITE")] },
    { call_id: "1-SDbbb-22222222", final_status: 486, msgs: [msg(2_020_000, "c:1", "b:1", "INVITE")] }
  ],
  groups: [
    { legs: [0, 1, 2], t0_us: 1_000_000, evidence: evidence.slice(0, 1) },
    { legs: [3, 4, 5], t0_us: 2_000_000, evidence: evidence.slice(1, 2) }
  ]
})

const stated = capture([
  { kind: "derived_call_id", strategy: 2, legs: [1, 2], prefix: "1-" },
  { kind: "derived_call_id", strategy: 2, legs: [4, 5], prefix: "1-" }
])

const unstated = capture([
  { kind: "shared_header_param", header: "P-Charging-Vector", legs: [0, 1, 2] },
  { kind: "shared_header_param", header: "P-Charging-Vector", legs: [3, 4, 5] }
])

const read = (value: unknown) => {
  const result = parseFlowsDocument(value)
  if (!result.ok) throw new Error(result.reason)
  return result.document
}

describe("captureSelection", () => {
  it("takes the legs and messages the flow's observed coordinates cite", () => {
    const selection = captureSelection(document([
      send("s1", "A", "INVITE", { observed: { at_us: 0, leg: 1, msg: 0 } }),
      expectResponse("s2", "A", 486, "Busy Here", { observed: { at_us: 500, leg: 1, msg: 3 } }),
      send("s3", "B", "INVITE", { observed: { at_us: 900, leg: 4, msg: 0 } })
    ]))
    expect([...selection.legs].sort()).toEqual([1, 4])
    expect([...selection.messages].sort()).toEqual(["1/0", "1/3", "4/0"])
    expect(selection.stepByMessage.get(messageCoordinate(1, 3))).toBe("s2")
    expect(selection.empty).toBe(false)
  })

  it("reads the coordinates inside an alt branch, which a case still consumed", () => {
    const selection = captureSelection(document([
      send("s1", "A", "INVITE", { observed: { at_us: 0, leg: 0, msg: 0 } }),
      {
        id: "a1",
        op: "alt",
        branches: [
          { name: "answered", steps: [expectResponse("s2", "A", 200, "OK", { observed: { at_us: 1, leg: 2, msg: 1 } })] },
          { name: "busy", steps: [expectResponse("s3", "A", 486, "Busy Here", { observed: { at_us: 2, leg: 3, msg: 1 } })] }
        ]
      }
    ]))
    expect([...selection.legs].sort()).toEqual([0, 2, 3])
  })

  it("says a document citing no coordinate is empty, so nothing reads as discarded", () => {
    const selection = captureSelection(document([send("s1", "A", "INVITE")]))
    expect(selection.empty).toBe(true)
    expect(selection.legs.size).toBe(0)
  })
})

describe("documentSut", () => {
  it("states the sockets of the document's sut-side endpoints, in document order, and no peer's", () => {
    const stated = document([send("s1", "A", "INVITE")], {
      endpoints: [
        { id: "ep0", side: "peer", binding: "loopback", observed: "10.0.0.7:5060" },
        { id: "ep1", side: "sut", binding: "loopback", observed: "10.0.0.2:5060" },
        { id: "ep2", side: "sut", binding: "loopback", observed: "10.0.0.2:5061" }
      ]
    })
    expect(documentSut(stated)).toEqual(["10.0.0.2:5060", "10.0.0.2:5061"])
  })

  it("states nothing where every endpoint is a peer's", () => {
    expect(documentSut(document([send("s1", "A", "INVITE")]))).toEqual([])
  })
})

describe("callIdFamilies", () => {
  it("joins a derived Call-ID to its base as the document's own evidence states it", () => {
    const families = callIdFamilies(read(stated))
    expect(families.families.map((family) => family.legs)).toEqual([[0], [1, 2], [3], [4, 5]])
    expect(families.byLeg.get(2)).toMatchObject({ derivedFrom: 1, derivedBy: "evidence", shade: 1 })
    expect(families.byLeg.get(1)).toMatchObject({ derivedFrom: undefined, shade: 0 })
    expect(families.warnings).toEqual([])
  })

  it("gives a family one hue and its legs different shades", () => {
    const families = callIdFamilies(read(stated))
    const base = families.byLeg.get(1)!
    const derived = families.byLeg.get(2)!
    expect(derived.hueSlot).toBe(base.hueSlot)
    expect(derived.shade).not.toBe(base.shade)
    expect(callIdShadeClass(derived.shade)).not.toBe(callIdShadeClass(base.shade))
  })

  it("gives the two ingress calls of one capture different hues", () => {
    const families = callIdFamilies(read(stated))
    expect(families.byLeg.get(0)!.hueSlot).not.toBe(families.byLeg.get(3)!.hueSlot)
    expect(families.byLeg.get(1)!.hueSlot).not.toBe(families.byLeg.get(4)!.hueSlot)
  })

  it("falls back to the literal affix, with a warning, where the document states no derivation", () => {
    const families = callIdFamilies(read(unstated))
    expect(families.families.map((family) => family.legs)).toEqual([[0], [1, 2], [3], [4, 5]])
    expect(families.byLeg.get(2)).toMatchObject({ derivedFrom: 1, derivedBy: "literal" })
    expect(families.warnings.map((one) => one.text).join(" ")).toContain("literal affix")
  })

  it("warns instead of guessing when evidence names a leg the document does not hold", () => {
    const families = callIdFamilies(read(capture([{ kind: "derived_call_id", legs: [9] }])))
    expect(families.warnings.map((one) => one.text).join(" ")).toContain("does not hold")
  })

  it("reads the affix relation in either direction and refuses an unrelated pair", () => {
    expect(extendsCallId("SDaaa", "1-SDaaa")).toBe(true)
    expect(extendsCallId("SDaaa", "SDaaa-b2b")).toBe(true)
    expect(extendsCallId("SDaaa", "SDaaa")).toBe(false)
    expect(extendsCallId("SDaaa", "SDbbb-1")).toBe(false)
  })

  it("keeps a Call-ID's head and tail when it truncates, because both carry the derivation", () => {
    const short = shortCallId("1-SDnbrub01-7cce5ec0ef9099f1f11b152bea3e9e14-v300g00020")
    expect(short.startsWith("1-SDnbrub01-")).toBe(true)
    expect(short.endsWith("00020")).toBe(true)
    expect(short.length).toBe(24)
    expect(shortCallId("short-one")).toBe("short-one")
  })
})

describe("buildFlowsLayout triage", () => {
  const selection = captureSelection(document([
    send("s1", "A", "INVITE", { observed: { at_us: 0, leg: 1, msg: 0 } }),
    send("s2", "B", "INVITE", { observed: { at_us: 10, leg: 2, msg: 0 } }),
    send("s3", "C", "INVITE", { observed: { at_us: 20, leg: 5, msg: 0 } })
  ]))

  it("draws the WHOLE capture by default, discarded legs included", () => {
    const layout = buildFlowsLayout(read(stated), { groups: [0], selection })
    expect(layout.rows).toHaveLength(6)
    expect(layout.legs.map((leg) => leg.legIndex)).toEqual([0, 1, 2, 3, 4, 5])
  })

  it("marks the cited legs selected and every other leg discarded", () => {
    const layout = buildFlowsLayout(read(stated), { selection })
    expect(layout.hasSelection).toBe(true)
    expect(layout.legs.filter((leg) => leg.selected).map((leg) => leg.legIndex)).toEqual([1, 2, 5])
    expect(layout.rows.filter((row) => !row.selected).map((row) => row.legIndex)).toEqual([0, 3, 4])
    expect(layout.rows.find((row) => row.legIndex === 1)?.stepId).toBe("s1")
  })

  it("restricts to the case's call groups only when asked, and keeps the selection", () => {
    const layout = buildFlowsLayout(read(stated), { groups: [0], restrictToGroups: true, selection })
    expect(layout.legs.map((leg) => leg.legIndex)).toEqual([0, 1, 2])
    expect(layout.legs.filter((leg) => leg.selected).map((leg) => leg.legIndex)).toEqual([1, 2])
  })

  it("draws only the selected legs when asked, and says how much it held back", () => {
    const layout = buildFlowsLayout(read(stated), { selection, onlySelected: true })
    expect(layout.legs.map((leg) => leg.legIndex)).toEqual([1, 2, 5])
    expect(layout.rows.every((row) => row.selected)).toBe(true)
    expect(layout.hiddenMessages).toBe(3)
    expect(layout.subtitle).toContain("3 messages hidden")
  })

  it("keeps, on onlySelected, only the family warnings naming a leg it draws", () => {
    // `unstated` joins 1-2 and 4-5 by the literal affix, one warning each;
    // a ladder of legs 1, 2 and 5 draws both pairs' legs, one of 1 and 2 only one.
    const both = buildFlowsLayout(read(unstated), { selection, onlySelected: true })
    expect(both.warnings.filter((one) => one.includes("literal affix"))).toHaveLength(2)
    const one = buildFlowsLayout(read(unstated), {
      selection: captureSelection(document([
        send("s1", "A", "INVITE", { observed: { at_us: 0, leg: 1, msg: 0 } }),
        send("s2", "B", "INVITE", { observed: { at_us: 10, leg: 2, msg: 0 } })
      ])),
      onlySelected: true
    })
    expect(one.warnings.filter((one) => one.includes("literal affix"))).toEqual([
      "leg 2's Call-ID extends leg 1's; joined by the literal affix, the document states no derived_call_id evidence for it"
    ])
    // Drawn whole, every warning stands.
    const whole = buildFlowsLayout(read(unstated), { selection })
    expect(whole.warnings.filter((one) => one.includes("literal affix"))).toHaveLength(2)
  })

  it("holds nothing back on onlySelected where the document cites no coordinate", () => {
    const layout = buildFlowsLayout(read(stated), { onlySelected: true })
    expect(layout.legs.map((leg) => leg.legIndex)).toEqual([0, 1, 2, 3, 4, 5])
    expect(layout.hiddenMessages).toBe(0)
  })

  it("draws every leg selected where the document cites no coordinate", () => {
    const layout = buildFlowsLayout(read(stated))
    expect(layout.hasSelection).toBe(false)
    expect(layout.legs.every((leg) => leg.selected)).toBe(true)
  })

  it("says why an unselected leg rides one of the case's groups", () => {
    const doc = capture([
      { kind: "shared_header_param", header: "P-Charging-Vector", param: "icid-value", legs: [0, 1, 2] },
      { kind: "shared_header_param", header: "P-Charging-Vector", param: "icid-value", legs: [3, 4, 5] }
    ])
    const layout = buildFlowsLayout(read(doc), { groups: [0], selection })
    const legOf = (index: number) => layout.legs.find((leg) => leg.legIndex === index)!
    expect(legOf(0).rideAlong).toBe(
      "in the case's group 0 by P-Charging-Vector icid-value match; not part of the cut"
    )
    // A selected leg, and an unselected leg outside the case's groups, say nothing.
    expect(legOf(1).rideAlong).toBeUndefined()
    expect(legOf(3).rideAlong).toBeUndefined()
    // No selection, no ride-along: nothing reads as discarded either.
    expect(
      buildFlowsLayout(read(doc), { groups: [0] }).legs.every((leg) => leg.rideAlong === undefined)
    ).toBe(true)
  })

  it("states each leg's Call-ID, message count and first-message time in its header", () => {
    const layout = buildFlowsLayout(read(stated), { selection })
    expect(layout.legs[3]).toMatchObject({
      legIndex: 3,
      callId: "ingress-two",
      messageCount: 1,
      firstAtMs: 1000,
      finalStatus: 486,
      selected: false
    })
    expect(layout.legs[2]?.shortCallId).toBe("1-SDaaa-11111111")
  })

  it("keeps the leg band clear of the sockets and the rows", () => {
    const layout = buildFlowsLayout(read(stated), { selection })
    expect(layout.socketBoxY).toBeGreaterThan(10)
    expect(layout.lifelineTop).toBeGreaterThan(layout.socketBoxY)
    expect(layout.rows[0]!.y).toBeGreaterThan(layout.lifelineTop)
  })
})
