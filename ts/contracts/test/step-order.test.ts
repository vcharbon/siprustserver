/** A document's step order: every flow node with an id and an op, depth first; nothing from what it cannot read. */
import { describe, expect, it } from "vitest"
import { stepOrderOf } from "../src/step-order.js"

describe("the step order of a case document", () => {
  it("lists every flow node carrying an id and an op, nested ones in place", () => {
    const document = {
      flow: [
        { id: "s1", op: "send" },
        { parallel: [{ id: "s2", op: "expect" }, { id: "s3", op: "expect" }] },
        { id: "s4", op: "send", msg: { id: "not-a-step" } }
      ]
    }
    expect(stepOrderOf(document)).toEqual(["s1", "s2", "s3", "s4"])
  })

  it("yields none for a document it does not recognise", () => {
    expect(stepOrderOf(null)).toEqual([])
    expect(stepOrderOf({ flow: "s1" })).toEqual([])
    expect(stepOrderOf([{ id: "s1", op: "send" }])).toEqual([])
  })
})
