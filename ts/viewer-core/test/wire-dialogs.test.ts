import { describe, expect, it } from "vitest"
import { wireDialogs } from "../src/flows/dialogs.ts"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { EMPTY_SELECTION, type CaptureSelection } from "../src/flows/selection.ts"

const message = (
  tsUs: number,
  src: string,
  dst: string,
  summary: Record<string, unknown>
) => ({ ts_us: tsUs, src, dst, hop: 0, retx: false, raw: "", summary })

const tags = (from: string | null, to: string | null) => ({
  from: { uri: "sip:caller@host", tag: from },
  to: { uri: "sip:callee@host", tag: to }
})

/** A forked leg: one INVITE answered under two To-tags, then a BYE the callee sends. */
const doc = {
  schema: 5,
  legs: [
    {
      call_id: "call-a",
      final_status: 200,
      msgs: [
        message(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", {
          kind: "request",
          method: "INVITE",
          cseq: { seq: 1, method: "INVITE" },
          ...tags("caller-tag", null)
        }),
        message(1_001_000, "10.0.0.9:5060", "10.0.0.1:5060", {
          kind: "response",
          status: 180,
          reason: "Ringing",
          cseq: { seq: 1, method: "INVITE" },
          ...tags("caller-tag", "fork-one")
        }),
        message(1_002_000, "10.0.0.9:5060", "10.0.0.1:5060", {
          kind: "response",
          status: 180,
          reason: "Ringing",
          cseq: { seq: 1, method: "INVITE" },
          ...tags("caller-tag", "fork-two")
        }),
        message(1_003_000, "10.0.0.9:5060", "10.0.0.1:5060", {
          kind: "response",
          status: 200,
          reason: "OK",
          cseq: { seq: 1, method: "INVITE" },
          ...tags("caller-tag", "fork-two")
        }),
        // The callee's BYE: the leg's originator tag rides To, not From.
        message(1_004_000, "10.0.0.9:5060", "10.0.0.1:5060", {
          kind: "request",
          method: "BYE",
          cseq: { seq: 1, method: "BYE" },
          ...tags("fork-two", "caller-tag")
        })
      ]
    },
    {
      call_id: "call-b",
      msgs: [
        message(1_005_000, "10.0.0.9:5060", "10.0.0.2:5060", {
          kind: "request",
          method: "INVITE",
          cseq: { seq: 1, method: "INVITE" },
          ...tags("sut-tag", null)
        })
      ]
    }
  ],
  groups: [{ legs: [0] }, { legs: [1] }]
}

const read = () => {
  const parsed = parseFlowsDocument(doc)
  if (!parsed.ok) throw new Error(parsed.reason)
  return parsed.document
}

/** A case that sent leg 0's INVITE and expected leg 0's 200: the SUT is 10.0.0.9. */
const selection: CaptureSelection = {
  ...EMPTY_SELECTION,
  legs: new Set([0]),
  messages: new Set(["0/0", "0/3"]),
  stepByMessage: new Map([["0/0", "s1"], ["0/3", "s2"]]),
  opByMessage: new Map<string, "send" | "expect">([["0/0", "send"], ["0/3", "expect"]]),
  empty: false
}

describe("parseFlowsDocument", () => {
  it("carries the wire tags the extractor read, so nothing downstream parses SIP", () => {
    const first = read().legs[0]?.messages[0]
    expect(first?.fromTag).toBe("caller-tag")
    expect(first?.toTag).toBeUndefined()
  })
})

describe("wireDialogs", () => {
  it("names one from-tag per leg and one to-tag per answering tag", () => {
    const document = read()
    const dialogs = wireDialogs(document, document.messages)
    const leg = dialogs.byLeg.get(0)
    expect(leg?.from?.value).toBe("caller-tag")
    expect(leg?.to.map((tag) => [tag.token, tag.value])).toEqual([
      ["to0.1", "fork-one"],
      ["to0.2", "fork-two"]
    ])
    expect(leg?.forked).toBe(true)
    expect(dialogs.forkedLegs).toEqual([0])
  })

  it("drops the fork suffix on a leg that answered under one tag", () => {
    const document = read()
    const dialogs = wireDialogs(document, document.messages)
    expect(dialogs.byLeg.get(1)?.to).toEqual([])
    expect(dialogs.byLeg.get(1)?.from?.token).toBe("from1")
  })

  it("leaves the to-bead absent while nobody has answered", () => {
    const document = read()
    const dialogs = wireDialogs(document, document.messages)
    const invite = dialogs.byMessage.get("0/0")
    expect(invite?.to).toBeUndefined()
    expect(invite?.ordinal).toBe(0)
  })

  it("reads a reverse-direction request as originator against answerer, not From against To", () => {
    const document = read()
    const dialogs = wireDialogs(document, document.messages)
    const bye = dialogs.byMessage.get("0/4")
    expect(bye?.from?.value).toBe("caller-tag")
    // The BYE rides the fork that answered, so it keeps that fork's colour.
    expect(bye?.to?.token).toBe("to0.2")
    expect(bye?.unanchored).toBe(false)
  })

  it("widens the short form until a leg's tags read apart, and keeps it bead-sized", () => {
    // Two forks of one platform: same prefix, same suffix, eight bytes apart.
    const collide = {
      schema: 5,
      legs: [{
        call_id: "call-c",
        msgs: [
          message(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", {
            kind: "request",
            method: "INVITE",
            ...tags("SDoke3f99-SD3gn0b99-0084c10d-0006-0017", null)
          }),
          message(1_001_000, "10.0.0.9:5060", "10.0.0.1:5060", {
            kind: "response",
            status: 180,
            ...tags("SDoke3f99-SD3gn0b99-0084c10d-0006-0017", "SDsame01-SD3gn0b99-0084c10d-0006-0017")
          }),
          message(1_002_000, "10.0.0.9:5060", "10.0.0.1:5060", {
            kind: "response",
            status: 180,
            ...tags("SDoke3f99-SD3gn0b99-0084c10d-0006-0017", "SDsame01-SD3gn0b99-0084c238-0006-0017")
          })
        ]
      }],
      groups: [{ legs: [0] }]
    }
    const parsed = parseFlowsDocument(collide)
    if (!parsed.ok) throw new Error(parsed.reason)
    const leg = wireDialogs(parsed.document, parsed.document.messages).byLeg.get(0)
    const shorts = [leg?.from?.short, ...(leg?.to ?? []).map((tag) => tag.short)]
    expect(new Set(shorts).size).toBe(3)
    for (const short of shorts) expect(short?.length).toBeLessThanOrEqual(16)
  })

  it("keeps a leg's originator tag when a socket filter hides its first message", () => {
    const document = read()
    const dialogs = wireDialogs(document, document.messages.filter((m) => m.msgIndex > 0))
    expect(dialogs.byLeg.get(0)?.from?.value).toBe("caller-tag")
  })
})

describe("buildFlowsLayout", () => {
  it("names the SUT's socket off the case's own send/expect steps", () => {
    const layout = buildFlowsLayout(read(), { selection })
    expect(layout.sutSockets).toEqual(["10.0.0.9:5060"])
    expect(layout.lifelines.find((line) => line.id === "10.0.0.9:5060")?.sut).toBe(true)
    expect(layout.sockets.map((socket) => [socket.id, socket.sut])).toEqual([
      ["10.0.0.1:5060", false],
      ["10.0.0.2:5060", false],
      ["10.0.0.9:5060", true]
    ])
  })

  it("names no SUT where the case cites no capture coordinate", () => {
    expect(buildFlowsLayout(read()).sutSockets).toEqual([])
    expect(buildFlowsLayout(read()).subtitle).toContain("no SUT socket")
  })

  it("draws only what a picked socket exchanged, and still offers the rest", () => {
    const layout = buildFlowsLayout(read(), { selection, sockets: ["10.0.0.2:5060"] })
    expect(layout.rows.map((row) => row.id)).toEqual(["1/0"])
    expect(layout.lifelines.map((line) => line.id)).toEqual(["10.0.0.2:5060", "10.0.0.9:5060"])
    // Every socket stays clickable: a chip that vanishes cannot be clicked off.
    expect(layout.sockets.map((socket) => socket.id)).toEqual([
      "10.0.0.1:5060",
      "10.0.0.2:5060",
      "10.0.0.9:5060"
    ])
    expect(layout.sockets.find((socket) => socket.id === "10.0.0.1:5060")?.drawn).toBe(false)
  })

  it("draws one leg alone when it is picked, and keeps every leg's chip clickable", () => {
    const layout = buildFlowsLayout(read(), { selection, legs: [1] })
    expect(layout.rows.map((row) => row.id)).toEqual(["1/0"])
    expect(layout.legs.map((leg) => [leg.legIndex, leg.drawn])).toEqual([[0, false], [1, true]])
    // The chip keeps the leg's own size, not the filtered drawing's: a count
    // that fell to zero would say the leg had nothing to come back to.
    expect(layout.legs.find((leg) => leg.legIndex === 0)?.messageCount).toBe(5)
    expect(layout.subtitle).toContain("filtered to leg 1")
  })

  it("puts each row's tag pair on the row, and names the forks in the subtitle", () => {
    const layout = buildFlowsLayout(read(), { selection })
    expect(layout.rows.find((row) => row.id === "0/1")?.dialog?.to?.token).toBe("to0.1")
    expect(layout.rows.find((row) => row.id === "0/2")?.dialog?.to?.token).toBe("to0.2")
    expect(layout.subtitle).toContain("1 forked leg(s): 0")
    expect(layout.subtitle).toContain("SUT at 10.0.0.9:5060")
  })
})
