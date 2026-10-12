import { describe, expect, it } from "vitest"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { isMessageEvent, SUT_LIFELINE, type MessageEvent } from "../src/layout/model.ts"
import { document, expectRequest, expectResponse, send, delay } from "./documents.ts"

const messages = (events: readonly { kind: string }[]): readonly MessageEvent[] =>
  events.filter((event): event is MessageEvent => isMessageEvent(event as never))

describe("linear flow", () => {
  const layout = buildScenarioLayout(document([
    send("s1", "A", "INVITE"),
    expectResponse("s2", "A", 100, "Trying", { auto: true, check: "record" }),
    expectRequest("s3", "B", "INVITE"),
    send("s4", "B", "200")
  ]))

  it("places the originating actor left, the SUT in the middle and the callee right", () => {
    expect(layout.lifelines.map((lifeline) => lifeline.id)).toEqual(["uac1", SUT_LIFELINE, "uas1"])
    expect(layout.lifelines[1]!.kind).toBe("sut")
  })

  it("gives every flow node its own row, in document order", () => {
    expect(layout.events.map((event) => event.id)).toEqual(["s1", "s2", "s3", "s4"])
    expect(layout.events.map((event) => event.row)).toEqual([0, 1, 2, 3])
    expect(layout.rowCount).toBe(4)
    const ys = layout.events.map((event) => event.y)
    expect([...ys].sort((left, right) => left - right)).toEqual(ys)
  })

  it("draws a send away from its actor and an expect towards it", () => {
    const [s1, s2] = messages(layout.events)
    expect(s1!.direction).toBe("toSut")
    expect(s1!.fromLifeline).toBe("uac1")
    expect(s1!.toLifeline).toBe(SUT_LIFELINE)
    expect(s2!.direction).toBe("fromSut")
    expect(s2!.fromLifeline).toBe(SUT_LIFELINE)
    expect(s2!.toLifeline).toBe("uac1")
  })

  it("anchors cross-leg arrows on different lifelines", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    const uac = layout.lifelines[0]!.x
    const sut = layout.lifelines[1]!.x
    const uas = layout.lifelines[2]!.x
    expect([byId.get("s1")!.x1, byId.get("s1")!.x2]).toEqual([uac, sut])
    expect([byId.get("s3")!.x1, byId.get("s3")!.x2]).toEqual([sut, uas])
    expect([byId.get("s4")!.x1, byId.get("s4")!.x2]).toEqual([uas, sut])
    expect(byId.get("s1")!.x1).not.toBe(byId.get("s3")!.x2)
  })

  it("labels a response with its status and reason", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    expect(byId.get("s2")!.label).toBe("100 Trying")
    expect(byId.get("s1")!.label).toBe("INVITE")
  })

  it("reports no warnings for a well-formed document", () => {
    expect(layout.warnings).toEqual([])
  })
})

describe("alt branches", () => {
  const layout = buildScenarioLayout(document([
    send("s1", "A", "INVITE"),
    {
      id: "a1",
      op: "alt",
      branches: [
        {
          name: "answered",
          steps: [
            expectResponse("s2", "A", 200, "OK"),
            send("s3", "A", "ACK", { auto: true }),
            send("s4", "A", "BYE")
          ]
        },
        { name: "cancelled", steps: [expectResponse("s5", "A", 487, "Request Terminated")] }
      ]
    }
  ]))

  it("gives each branch its own frame", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    expect(byId.get("s2")!.frameId).toBe("a1#answered")
    expect(byId.get("s5")!.frameId).toBe("a1#cancelled")
    expect(byId.get("s1")!.frameId).toBe("root")
  })

  it("stacks the branches: no two alternatives ever share a row", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    // alt header 0? no: s1 takes row 0, the alt header row 1, the first
    // section's heading row 2, so its steps run 3..5.
    expect([byId.get("s2")!.row, byId.get("s3")!.row, byId.get("s4")!.row]).toEqual([3, 4, 5])
    // Then an OR row, the second heading row, and the second branch's one step.
    expect(byId.get("s5")!.row).toBe(8)
    const rows = messages(layout.events).map((event) => event.row)
    expect(new Set(rows).size).toBe(rows.length)
  })

  it("frames every branch full width on the root's own lifelines", () => {
    const root = layout.frames["root"]!
    for (const name of ["answered", "cancelled"]) {
      const frame = layout.frames[`a1#${name}`]!
      expect([frame.x0, frame.x1]).toEqual([root.x0, root.x1])
      expect(frame.lifelineX).toEqual(root.lifelineX)
    }
    expect(layout.frames["a1#answered"]!.branchIndex).toBe(0)
    expect(layout.frames["a1#cancelled"]!.branchIndex).toBe(1)
  })

  it("draws a branch arrow at the same length as a shared-prefix one", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    const span = (id: string): number => Math.abs(byId.get(id)!.x2 - byId.get(id)!.x1)
    expect(span("s2")).toBe(span("s1"))
    expect(span("s5")).toBe(span("s1"))
    expect(byId.get("s2")!.altId).toBe("a1")
    expect(byId.get("s2")!.branchName).toBe("answered")
  })

  it("gives each section its own rows, an OR mark above all but the first, and a rejoin below", () => {
    const block = layout.blocks.find((entry) => entry.id === "a1")!
    expect(block.kind).toBe("alt")
    const [first, second] = block.branches
    expect(first!.index).toBe(0)
    expect([first!.rowStart, first!.rowEnd]).toEqual([2, 6])
    expect([second!.rowStart, second!.rowEnd]).toEqual([7, 9])
    expect(first!.separatorY).toBeUndefined()
    expect(second!.separatorY).toBeGreaterThan(first!.y1)
    expect(second!.y0).toBeGreaterThan(second!.separatorY!)
    expect(block.rejoinY).toBeGreaterThan(second!.y1)
    expect([block.rowStart, block.rowEnd]).toEqual([1, 10])
    expect(layout.rowCount).toBe(10)
  })

  it("resumes the shared flow full width under the rejoin", () => {
    const after = buildScenarioLayout(document([
      send("s1", "A", "INVITE"),
      {
        id: "a1",
        op: "alt",
        branches: [
          { name: "one", steps: [expectResponse("s2", "A", 200, "OK")] },
          { name: "two", steps: [expectResponse("s3", "A", 487, "Request Terminated")] }
        ]
      },
      send("s4", "A", "BYE")
    ]))
    const byId = new Map(messages(after.events).map((event) => [event.id, event]))
    const block = after.blocks.find((entry) => entry.id === "a1")!
    expect(byId.get("s4")!.frameId).toBe("root")
    expect(byId.get("s4")!.row).toBe(block.rowEnd)
    expect(byId.get("s4")!.y).toBeGreaterThan(block.rejoinY!)
  })
})

describe("unordered groups", () => {
  const layout = buildScenarioLayout(document([
    send("s1", "A", "INVITE"),
    {
      id: "u1",
      op: "unordered",
      steps: [expectRequest("s2", "B", "CANCEL"), expectRequest("s3", "B", "ACK", { auto: true })]
    },
    send("s4", "A", "BYE")
  ]))

  it("brackets its members over consecutive rows", () => {
    const block = layout.blocks.find((entry) => entry.id === "u1")!
    expect(block.kind).toBe("unordered")
    expect([block.rowStart, block.rowEnd]).toEqual([1, 3])
    expect(block.y1).toBeGreaterThan(block.y0)
    expect(block.branches).toEqual([])
  })

  it("tags every member with the group and leaves them in the root frame", () => {
    const grouped = messages(layout.events).filter((event) => event.groupId === "u1")
    expect(grouped.map((event) => event.id)).toEqual(["s2", "s3"])
    expect(grouped.every((event) => event.frameId === "root")).toBe(true)
  })

  it("resumes the flow after the group", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    expect(byId.get("s4")!.row).toBe(3)
  })
})

describe("inject and decorations", () => {
  const layout = buildScenarioLayout(document(
    [
      send("s1", "A", "INVITE", { delay: delay(700, "step:s0"), retransmits: 2 }),
      { id: "i1", op: "inject", action: "store-fault:LiveAudit", target: "consultation-dialog", after: ["s1"] },
      expectResponse("s2", "A", 200, "OK", { checks: [{ field: "to.tag", op: "exists" }], within_ms: 500 })
    ],
    { deviations: [{ id: "d1", kind: "cseq-override", leg: "A", step: "s2", value: 7 }] }
  ))

  it("draws an inject as its own annotated event", () => {
    const event = layout.events.find((entry) => entry.id === "i1")!
    expect(event.kind).toBe("inject")
    expect(event.row).toBe(1)
    if (event.kind !== "inject") throw new Error("expected an inject event")
    expect(event.action).toBe("store-fault:LiveAudit")
    expect(event.decorations.some((decoration) => decoration.kind === "after")).toBe(true)
  })

  it("decorates a step with its dwell, its retransmits and its deviations", () => {
    const byId = new Map(messages(layout.events).map((event) => [event.id, event]))
    const s1 = byId.get("s1")!.decorations
    expect(s1).toContainEqual({ kind: "delay", ms: 700, from: "step:s0", compressible: true, timerLinked: false })
    expect(s1).toContainEqual({ kind: "retransmits", count: 2 })
    const s2 = byId.get("s2")!.decorations
    expect(s2).toContainEqual({ kind: "deviation", deviationId: "d1", deviationKind: "cseq-override" })
    expect(s2).toContainEqual({ kind: "checks", count: 1 })
    expect(s2).toContainEqual({ kind: "within", ms: 500 })
  })
})

describe("malformed references", () => {
  it("warns rather than throwing when a step names an undeclared leg", () => {
    const layout = buildScenarioLayout(document([send("s1", "Z", "INVITE")]))
    expect(layout.warnings.length).toBeGreaterThan(0)
    expect(layout.events).toHaveLength(1)
  })
})
