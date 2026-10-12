import { describe, expect, it } from "vitest"
import { callBands, callSlotClass, CALL_SLOT_COUNT } from "../src/layout/calls.ts"
import { isMessageEvent } from "../src/layout/model.ts"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { document, expectRequest, send } from "./documents.ts"

const twoCalls = {
  identities: [
    { name: "c1-called-0-0", kind: "site", observed: "+15550100004" },
    { name: "c2-called-0-0", kind: "site", observed: "+15550100005" },
    { name: "early", kind: "site", observed: "a" },
    { name: "late", kind: "site", observed: "b" }
  ],
  calls: [
    {
      id: "c1",
      caller_leg: "A",
      attempts: [{
        branch: 0,
        position: 0,
        leg: "B",
        callee: { identity: "c1-called-0-0" },
        final: { status: 200, at_ms: 60 }
      }]
    },
    {
      id: "c2",
      caller_leg: "C",
      attempts: [{
        branch: 0,
        position: 0,
        leg: "D",
        callee: { identity: "c2-called-0-0" },
        cause: "busy" as const
      }]
    }
  ],
  endpoints: [
    { id: "ep0", observed: "127.0.0.1:5060", side: "peer" as const, binding: "dedicated" as const },
    { id: "ep1", observed: "127.0.0.1:5070", side: "peer" as const, binding: "dedicated" as const }
  ],
  actors: [
    { id: "uac1", kind: "uac" as const, endpoint: "ep0" },
    { id: "uas1", kind: "uas" as const, endpoint: "ep1" },
    { id: "uac2", kind: "uac" as const, endpoint: "ep0" },
    { id: "uas2", kind: "uas" as const, endpoint: "ep1" }
  ],
  legs: [
    { id: "A", actor: "uac1", dir: "out" as const },
    { id: "B", actor: "uas1", dir: "in" as const },
    { id: "C", actor: "uac2", dir: "out" as const },
    { id: "D", actor: "uas2", dir: "in" as const }
  ]
}

describe("call bands", () => {
  const doc = document(
    [send("s1", "A", "INVITE"), expectRequest("s2", "B", "INVITE"), send("s3", "C", "INVITE")],
    twoCalls
  )

  it("gives every call a slot from its position in calls[], so the colour never moves", () => {
    const bands = callBands(doc).bands
    expect(bands.map((band) => [band.id, band.slot])).toEqual([["c1", 0], ["c2", 1]])
  })

  it("collects the caller leg first, then the attempt legs", () => {
    expect(callBands(doc).bands.map((band) => band.legs)).toEqual([["A", "B"], ["C", "D"]])
  })

  it("indexes the calls by leg, which is what an arrow is coloured by", () => {
    const byLeg = callBands(doc).byLeg
    expect(byLeg.get("B")?.id).toBe("c1")
    expect(byLeg.get("C")?.id).toBe("c2")
    expect(byLeg.get("Z")).toBeUndefined()
  })

  it("summarises each attempt for the legend", () => {
    const [first, second] = callBands(doc).bands
    expect(first?.attempts[0]).toMatchObject({ leg: "B", callee: "+15550100004", finalStatus: 200 })
    expect(second?.attempts[0]?.cause).toBe("busy")
  })

  it("labels a joined leg, so its own branch does not read as a parallel fork", () => {
    const transfer = document([send("s1", "A", "INVITE")], {
      identities: [
        { name: "called-0-0", kind: "site", observed: "+15550100004" },
        { name: "called-1-0", kind: "site", observed: "+15550100006" }
      ],
      calls: [{
        id: "c1",
        caller_leg: "A",
        attempts: [
          { branch: 0, position: 0, leg: "B", callee: { identity: "called-0-0" } },
          {
            branch: 1,
            position: 0,
            leg: "C",
            callee: { identity: "called-1-0" },
            joined_by: { kind: "refer" as const, step: "s1" }
          }
        ]
      }]
    })
    const attempts = callBands(transfer).bands[0]?.attempts
    expect(attempts?.[0]?.joinedBy).toBeUndefined()
    expect(attempts?.[1]?.joinedBy).toEqual({ kind: "refer", step: "s1" })
  })

  it("orders attempts by (branch, position) rather than by array order", () => {
    const chained = document([send("s1", "A", "INVITE")], {
      ...twoCalls,
      calls: [{
        id: "c1",
        caller_leg: "A",
        attempts: [
          { branch: 0, position: 1, leg: "C", callee: { identity: "late" } },
          { branch: 0, position: 0, leg: "B", callee: { identity: "early" } }
        ]
      }]
    })
    expect(callBands(chained).bands[0]?.legs).toEqual(["A", "B", "C"])
  })

  it("warns instead of guessing when two calls claim one leg", () => {
    const shared = document([send("s1", "A", "INVITE")], {
      ...twoCalls,
      calls: [
        { id: "c1", caller_leg: "A", attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "early" } }] },
        { id: "c2", caller_leg: "C", attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "early" } }] }
      ]
    })
    const result = callBands(shared)
    expect(result.warnings).toHaveLength(1)
    expect(result.byLeg.get("B")?.id).toBe("c1")
  })

  it("wraps the palette rather than running out of classes", () => {
    expect(callSlotClass(0)).toBe("fv-call-0")
    expect(callSlotClass(CALL_SLOT_COUNT)).toBe("fv-call-0")
  })
})

describe("calls in the layout", () => {
  const layout = buildScenarioLayout(
    document(
      [send("s1", "A", "INVITE"), expectRequest("s2", "B", "INVITE"), send("s3", "C", "INVITE")],
      twoCalls
    )
  )

  it("tags each arrow with the call its leg belongs to", () => {
    const events = layout.events.filter(isMessageEvent)
    expect(events.map((event) => [event.id, event.callId, event.callSlot])).toEqual([
      ["s1", "c1", 0],
      ["s2", "c1", 0],
      ["s3", "c2", 1]
    ])
  })

  it("tags a lifeline with the calls its legs belong to", () => {
    const uac2 = layout.lifelines.find((lifeline) => lifeline.id === "uac2")
    expect(uac2?.callIds).toEqual(["c2"])
    expect(uac2?.callSlot).toBe(1)
  })

  it("publishes the bands so a legend can name the colours", () => {
    expect(layout.calls.map((call) => call.id)).toEqual(["c1", "c2"])
  })

  it("leaves a single-call document with one band, so nothing is tinted", () => {
    const single = buildScenarioLayout(document([send("s1", "A", "INVITE")]))
    expect(single.calls).toHaveLength(1)
  })
})
