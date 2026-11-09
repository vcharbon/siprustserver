/** A check leads to the first ladder message its flow step decorates; a check with no step, or an unmatched one, to none. */
import { describe, expect, it } from "vitest"
import { checkMessages } from "../src/campaign-check-message.ts"
import { ladder } from "./ladder-fixture.ts"

describe("the message a check was made at", () => {
  const events = ladder().events

  it("is the message its step decorates", () => {
    expect(checkMessages([{ step: "s3" }, { step: "s4" }], events)).toEqual(["B#1", "B#2"])
  })

  it("is the first send of a step a retransmission repeats", () => {
    expect(events.filter((e) => e.decorations.some((d) => d.kind === "step" && d.id === "s1")).map((e) => e.id))
      .toEqual(["A#1", "A#2"])
    expect(checkMessages([{ step: "s1" }], events)).toEqual(["A#1"])
  })

  it("is none for a check with no step or a step no message carries", () => {
    expect(checkMessages([{ step: null }, {}, { step: "s9" }], events)).toEqual([undefined, undefined, undefined])
    expect(checkMessages([{ step: "s1" }], [])).toEqual([undefined])
  })
})
