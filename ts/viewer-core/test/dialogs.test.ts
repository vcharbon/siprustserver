import { describe, expect, it } from "vitest"
import { dialogTags } from "../src/layout/dialogs.ts"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { isMessageEvent } from "../src/layout/model.ts"
import { document, expectRequest, expectResponse, send } from "./documents.ts"

type Doc = Parameters<typeof dialogTags>[0]

/** The pair as the ladder states it: `fromA1 · toA1`, or the from-token alone. */
const pairOf = (doc: Doc, stepId: string): string | undefined =>
  dialogTags(doc).byStep.get(stepId)?.token

describe("dialog tag pairs", () => {
  const linear = document([
    send("s1", "A", "INVITE"),
    expectResponse("s2", "A", 100, "Trying"),
    expectRequest("s3", "B", "INVITE"),
    send("s4", "B", "180", { msg: { status: 180, "cseq-method": "INVITE" } }),
    expectResponse("s5", "A", 200, "OK"),
    send("s6", "A", "ACK"),
    expectRequest("s7", "B", "BYE")
  ])

  it("states both tags of a dialog: the originator's From and the answerer's To", () => {
    expect(pairOf(linear, "s5")).toBe("fromA1 · toA1")
    expect(pairOf(linear, "s4")).toBe("fromB1 · toB1")
  })

  it("names each token after its leg, whichever party holds the tag", () => {
    const tags = dialogTags(linear)
    // Leg A originates at its actor, so the actor holds From and the SUT holds To.
    expect(tags.byStep.get("s5")?.from.owner).toBe("actor")
    expect(tags.byStep.get("s5")?.to?.owner).toBe("sut")
    // Leg B is originated by the SUT, so the sides swap.
    expect(tags.byStep.get("s4")?.from.owner).toBe("sut")
    expect(tags.byStep.get("s4")?.to?.owner).toBe("actor")
  })

  it("gives requests and responses of one dialog the same pair", () => {
    expect(pairOf(linear, "s5")).toBe(pairOf(linear, "s6"))
    expect(pairOf(linear, "s4")).toBe(pairOf(linear, "s7"))
  })

  it("drops the To-token while no answerer has stated a tag", () => {
    expect(pairOf(linear, "s1")).toBe("fromA1")
    expect(pairOf(linear, "s2")).toBe("fromA1")
    expect(pairOf(linear, "s3")).toBe("fromB1")
    expect(dialogTags(linear).byStep.get("s1")?.to).toBeUndefined()
  })

  it("lists one from-token and one to-token per leg when nothing forked", () => {
    const tags = dialogTags(linear)
    expect(tags.all.map((token) => token.token)).toEqual(["fromA1", "toA1", "fromB1", "toB1"])
    expect(tags.forkedLegs).toEqual([])
  })

  it("keeps a single early dialog as the leg's only to-token", () => {
    const doc = document([
      send("s1", "A", "INVITE"),
      expectRequest("s2", "B", "INVITE"),
      send("s3", "B", "183", { early: "f1", msg: { status: 183, "cseq-method": "INVITE" } }),
      expectRequest("s4", "B", "PRACK", { early: "f1" })
    ])
    expect(pairOf(doc, "s3")).toBe("fromB1 · toB1")
    expect(pairOf(doc, "s4")).toBe("fromB1 · toB1")
    expect(dialogTags(doc).forkedLegs).toEqual([])
  })

  const forking = document([
    send("s1", "A", "INVITE"),
    expectResponse("s2", "A", 100, "Trying"),
    expectRequest("s3", "B", "INVITE"),
    send("s4", "B", "183", { early: "f1", msg: { status: 183, "cseq-method": "INVITE" } }),
    send("s5", "B", "180", { early: "f2", msg: { status: 180, "cseq-method": "INVITE" } }),
    expectRequest("s6", "B", "PRACK", { early: "f2" }),
    send("s7", "B", "200", { early: "f1", msg: { status: 200, "cseq-method": "INVITE" } }),
    expectRequest("s8", "B", "BYE")
  ])

  it("reads a fork as one from-token against one to-token per early dialog", () => {
    expect(pairOf(forking, "s4")).toBe("fromB1 · toB1")
    expect(pairOf(forking, "s5")).toBe("fromB1 · toB2")
    expect(pairOf(forking, "s6")).toBe("fromB1 · toB2")
    expect(pairOf(forking, "s7")).toBe("fromB1 · toB1")
  })

  it("orders the fork ordinals by first use", () => {
    const tags = dialogTags(forking)
    expect(tags.all.find((token) => token.token === "toB1")?.earlyId).toBe("f1")
    expect(tags.all.find((token) => token.token === "toB2")?.earlyId).toBe("f2")
  })

  it("leaves a forked leg tagless until one of its early dialogs states a tag", () => {
    expect(pairOf(forking, "s3")).toBe("fromB1")
  })

  it("keeps a forked leg on the tag of the early dialog that answered, past the fork", () => {
    // s7 answers on f1, so the BYE after it restates f1's tag (RFC 3261 §12.2.1.1).
    expect(pairOf(forking, "s8")).toBe("fromB1 · toB1")
  })

  it("names a confirmed dialog's later messages after the fork colour they took", () => {
    const tags = dialogTags(forking)
    expect(tags.byStep.get("s8")?.ordinal).toBe(1)
    expect(tags.byStep.get("s8")?.earlyId).toBe("f1")
    expect(tags.byStep.get("s8")?.forked).toBe(true)
  })

  it("leaves the bare to-token off the legend where no message states it", () => {
    const tags = dialogTags(forking)
    expect(tags.all.map((token) => token.token)).toEqual(["fromA1", "toA1", "fromB1", "toB1", "toB2"])
  })

  it("marks only the to-tokens of a forked leg as forked, never its from-token", () => {
    const tags = dialogTags(forking)
    expect(tags.forkedLegs).toEqual(["B"])
    expect(tags.all.filter((token) => token.forked).map((token) => token.token))
      .toEqual(["toB1", "toB2"])
    expect(tags.all.find((token) => token.token === "fromB1")?.forked).toBe(false)
  })

  it("keeps the bare to-token where the leg states a tag before naming an early dialog", () => {
    const doc = document([
      expectRequest("s1", "B", "INVITE"),
      send("s2", "B", "180", { msg: { status: 180, "cseq-method": "INVITE" } }),
      send("s3", "B", "183", { early: "f1", msg: { status: 183, "cseq-method": "INVITE" } }),
      send("s4", "B", "180", { early: "f2", msg: { status: 180, "cseq-method": "INVITE" } })
    ])
    expect(pairOf(doc, "s2")).toBe("fromB1 · toB")
    expect(dialogTags(doc).all.map((token) => token.token))
      .toEqual(["fromA1", "toA1", "fromB1", "toB", "toB1", "toB2"])
  })

  it("leaves a CANCEL tagless: it restates the To of the request it cancels", () => {
    const doc = document([
      send("s1", "A", "INVITE"),
      expectResponse("s2", "A", 180, "Ringing"),
      send("s3", "A", "CANCEL"),
      expectResponse("s4", "A", 487, "Request Terminated")
    ])
    expect(pairOf(doc, "s3")).toBe("fromA1")
    expect(pairOf(doc, "s4")).toBe("fromA1 · toA1")
  })

  it("keeps one alt branch's answer from establishing its sibling's dialog", () => {
    const doc = document([
      send("s1", "A", "INVITE"),
      {
        id: "a1",
        op: "alt",
        branches: [
          { name: "answered", steps: [expectResponse("s2", "A", 200, "OK"), send("s3", "A", "ACK")] },
          { name: "cancelled", steps: [send("s4", "A", "CANCEL"), expectResponse("s5", "A", 487, "Request Terminated")] }
        ]
      }
    ])
    expect(pairOf(doc, "s3")).toBe("fromA1 · toA1")
    expect(pairOf(doc, "s4")).toBe("fromA1")
  })

  it("reaches the steps inside alt branches and unordered groups", () => {
    const doc = document([
      {
        id: "u1",
        op: "unordered",
        steps: [
          expectRequest("s1", "B", "INVITE"),
          expectRequest("s2", "B", "PRACK", { early: "e1" })
        ]
      },
      {
        id: "a1",
        op: "alt",
        branches: [
          { name: "one", steps: [expectResponse("s3", "A", 200, "OK")] },
          { name: "two", steps: [expectResponse("s4", "A", 486, "Busy Here")] }
        ]
      }
    ])
    expect(pairOf(doc, "s2")).toBe("fromB1 · toB1")
    expect(pairOf(doc, "s4")).toBe("fromA1 · toA1")
  })

  it("states a leg's settled pair for a message matched to no step", () => {
    // The leg ended on f1's tag, so an unplaced message of it states f1's token.
    expect(dialogTags(forking).byLeg.get("B")?.token).toBe("fromB1 · toB1")
    expect(dialogTags(linear).byLeg.get("A")?.token).toBe("fromA1 · toA1")
  })

  it("leaves a leg the alt branches settled differently on no fork's tag", () => {
    const doc = document([
      expectRequest("s1", "B", "INVITE"),
      send("s2", "B", "183", { early: "f1", msg: { status: 183, "cseq-method": "INVITE" } }),
      send("s3", "B", "180", { early: "f2", msg: { status: 180, "cseq-method": "INVITE" } }),
      {
        id: "a1",
        op: "alt",
        branches: [
          { name: "f1 answers", steps: [send("s4", "B", "200", { early: "f1", msg: { status: 200, "cseq-method": "INVITE" } })] },
          { name: "f2 answers", steps: [send("s5", "B", "200", { early: "f2", msg: { status: 200, "cseq-method": "INVITE" } })] }
        ]
      },
      expectRequest("s6", "B", "ACK")
    ])
    expect(pairOf(doc, "s4")).toBe("fromB1 · toB1")
    expect(pairOf(doc, "s5")).toBe("fromB1 · toB2")
    expect(pairOf(doc, "s6")).toBe("fromB1 · toB")
  })

  it("attaches the pair to the drawn event, so the ladder can label it", () => {
    const layout = buildScenarioLayout(forking)
    const at = (id: string) => layout.events.filter(isMessageEvent).find((event) => event.id === id)
    expect(at("s5")?.dialog?.token).toBe("fromB1 · toB2")
    expect(at("s5")?.dialog?.to?.ordinal).toBe(2)
    expect(at("s5")?.dialog?.forked).toBe(true)
    expect(at("s3")?.dialog?.to).toBeUndefined()
    expect(layout.dialogs.map((token) => token.token)).toContain("toB2")
  })
})
