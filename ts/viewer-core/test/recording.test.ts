import { describe, expect, it } from "vitest"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { isMessageEvent, type MessageEvent } from "../src/layout/model.ts"
import { buildRecordingLayout } from "../src/recording/layout.ts"
import { parseRecording, parseRecordingLeg } from "../src/recording/parse.ts"
import { recordedDialogTags } from "../src/recording/dialogs.ts"
import { parseSip } from "../src/recording/sip.ts"
import { compareLayouts } from "../src/compare/align.ts"
import { document, expectRequest, expectResponse, send } from "./documents.ts"

const invite = [
  "INVITE sip:+155500000000900004@example.test SIP/2.0",
  "Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1",
  "From: <sip:0009001@example.test>;tag=abc",
  "To: <sip:+155500000000900004@example.test>",
  "Call-ID: call-1@example.test",
  "CSeq: 1 INVITE",
  "Content-Length: 0",
  "",
  ""
].join("\r\n")

const trying = "SIP/2.0 100 Trying\r\nCall-ID: call-1@example.test\r\nCSeq: 1 INVITE\r\n\r\n"

describe("parseSip", () => {
  it("reads a request start line and its headers", () => {
    const parsed = parseSip(invite)
    expect(parsed.kind).toBe("request")
    expect(parsed.method).toBe("INVITE")
    expect(parsed.cseq).toBe(1)
    expect(parsed.cseqMethod).toBe("INVITE")
    expect(parsed.callId).toBe("call-1@example.test")
  })

  it("reads a response start line", () => {
    const parsed = parseSip(trying)
    expect(parsed.kind).toBe("response")
    expect(parsed.status).toBe(100)
    expect(parsed.reason).toBe("Trying")
  })

  it("reads the From-tag and the To-tag, absent where the header states none", () => {
    expect(parseSip(invite).fromTag).toBe("abc")
    expect(parseSip(invite).toTag).toBeUndefined()
    const answered = "SIP/2.0 200 OK\r\nFrom: <sip:a@x>;tag=abc\r\nTo: <sip:b@x>;tag=t1\r\nCSeq: 1 INVITE\r\n\r\n"
    expect(parseSip(answered).toTag).toBe("t1")
  })

  it("keeps a malformed message readable instead of failing", () => {
    const parsed = parseSip("NOT A SIP MESSAGE\r\nCSeq: 4 BYE\r\n\r\n")
    expect(parsed.kind).toBe("unparsed")
    expect(parsed.cseq).toBe(4)
  })
})

describe("parseRecordingLeg", () => {
  const jsonl = [
    JSON.stringify({ seq: 1, dir: "out", at_us: 0, step: "s1", raw: invite }),
    JSON.stringify({ seq: 2, dir: "in", at_ms: 2, raw: trying }),
    "",
    "{ not json",
    JSON.stringify({ seq: 3, dir: "sideways", at_us: 5, raw: trying }),
    JSON.stringify({ seq: 4, dir: "in", at_us: 9000, msg: { status: 200, reason: "OK", "cseq-method": "INVITE" } })
  ].join("\n")

  const parsed = parseRecordingLeg("A", jsonl)

  it("decodes verbatim messages and pre-decoded ones alike", () => {
    expect(parsed.messages.map((message) => message.seq)).toEqual([1, 2, 4])
    expect(parsed.messages[0]!.method).toBe("INVITE")
    expect(parsed.messages[0]!.stepId).toBe("s1")
    expect(parsed.messages[1]!.status).toBe(100)
    expect(parsed.messages[2]!.status).toBe(200)
    expect(parsed.messages[2]!.cseqMethod).toBe("INVITE")
  })

  it("normalises millisecond timestamps to microseconds", () => {
    expect(parsed.messages[1]!.atUs).toBe(2000)
  })

  it("reports every unreadable line instead of dropping it silently", () => {
    expect(parsed.warnings).toHaveLength(2)
    expect(parsed.warnings[0]).toContain("not JSON")
    expect(parsed.warnings[1]).toContain("direction")
  })

  it("defaults the leg to the file it came from", () => {
    expect(parsed.messages.every((message) => message.leg === "A")).toBe(true)
  })
})

describe("parseRecording", () => {
  it("orders the whole bundle by arrival across legs", () => {
    const recording = parseRecording({
      A: [
        JSON.stringify({ seq: 1, dir: "out", at_us: 0, raw: invite }),
        JSON.stringify({ seq: 2, dir: "in", at_us: 3000, raw: trying })
      ].join("\n"),
      B: JSON.stringify({ seq: 1, dir: "in", at_us: 1500, raw: invite })
    })
    expect(recording.legs).toEqual(["A", "B"])
    expect(recording.messages.map((message) => [message.leg, message.atUs])).toEqual([
      ["A", 0],
      ["B", 1500],
      ["A", 3000]
    ])
  })
})

describe("recording layout", () => {
  const doc = document([send("s1", "A", "INVITE"), expectRequest("s2", "B", "INVITE")])
  const recording = parseRecording({
    A: JSON.stringify({ seq: 1, dir: "out", at_us: 0, raw: invite }),
    B: JSON.stringify({ seq: 1, dir: "in", at_us: 1500, raw: invite })
  })
  const layout = buildRecordingLayout(recording, { document: doc })

  it("borrows the document's lifelines", () => {
    expect(layout.lifelines.map((lifeline) => lifeline.id)).toEqual(["uac1", "__sut__", "uas1"])
    expect(layout.warnings).toEqual([])
  })

  it("draws an emitted message away from its actor and an arriving one towards it", () => {
    const [first, second] = layout.events as readonly MessageEvent[]
    expect(first!.direction).toBe("toSut")
    expect(second!.direction).toBe("fromSut")
    expect(second!.actorId).toBe("uas1")
  })

  it("falls back to leg lifelines when the bundle carries no document", () => {
    const bare = buildRecordingLayout(recording)
    expect(bare.lifelines.map((lifeline) => lifeline.id)).toEqual(["A", "__sut__", "B"])
    expect(bare.warnings.some((warning) => warning.includes("no pivot.json"))).toBe(true)
  })
})

describe("recorded dialog tags", () => {
  const wire = (start: string, cseq: string, toTag?: string, fromTag = "abc"): string => [
    start,
    `From: <sip:0009001@example.test>;tag=${fromTag}`,
    `To: <sip:+155500000000900004@example.test>${toTag === undefined ? "" : `;tag=${toTag}`}`,
    "Call-ID: call-1@example.test",
    `CSeq: ${cseq}`,
    "Content-Length: 0",
    "",
    ""
  ].join("\r\n")
  const line = (seq: number, dir: "in" | "out", raw: string, step?: string): string =>
    JSON.stringify({ seq, dir, at_us: seq * 1000, ...(step === undefined ? {} : { step }), raw })

  // The incident shape: the 2xx under t1, a 481 to the CANCEL under t2, and the
  // close's ACK and BYE carrying the 481's tag rather than the 2xx's.
  const recording = parseRecording({
    A: [
      line(1, "out", wire("INVITE sip:x@example.test SIP/2.0", "1 INVITE"), "s1"),
      line(2, "in", wire("SIP/2.0 180 Ringing", "1 INVITE", "t1"), "s2"),
      line(3, "out", wire("CANCEL sip:x@example.test SIP/2.0", "1 CANCEL"), "s3"),
      line(4, "in", wire("SIP/2.0 200 OK", "1 INVITE", "t1"), "s4"),
      line(5, "in", wire("SIP/2.0 481 Call/Transaction Does Not Exist", "1 CANCEL", "t2")),
      line(6, "out", wire("ACK sip:x@example.test SIP/2.0", "1 ACK", "t2")),
      line(7, "out", wire("BYE sip:x@example.test SIP/2.0", "2 BYE", "t2")),
      // The far side's own BYE: the originator's tag rides To, the answerer's From.
      line(8, "in", wire("BYE sip:y@example.test SIP/2.0", "3 BYE", "abc", "t1"))
    ].join("\n")
  })
  const doc = document([
    send("s1", "A", "INVITE"),
    expectResponse("s2", "A", 180, "Ringing"),
    send("s3", "A", "CANCEL"),
    expectResponse("s4", "A", 200, "OK")
  ])
  const tokens = (layout: ReturnType<typeof buildRecordingLayout>): readonly string[] =>
    layout.events.filter(isMessageEvent).map((event) => event.dialog?.token ?? "-")

  it("aliases each distinct answering tag of a leg in first-seen order", () => {
    const tags = recordedDialogTags(recording)
    expect(tags.byMessage.get("A#4")?.token).toBe("fromA1 · toA1")
    expect(tags.byMessage.get("A#5")?.token).toBe("fromA1 · toA2")
    expect(tags.byMessage.get("A#6")?.token).toBe("fromA1 · toA2")
    expect(tags.byMessage.get("A#7")?.token).toBe("fromA1 · toA2")
    expect(tags.all.map((token) => [token.token, token.value])).toEqual([
      ["fromA1", "abc"],
      ["toA1", "t1"],
      ["toA2", "t2"]
    ])
    expect(tags.forkedLegs).toEqual(["A"])
  })

  it("leaves a tagless To without a to-token: the INVITE and the CANCEL read as fromA1 alone", () => {
    const tags = recordedDialogTags(recording)
    expect(tags.byMessage.get("A#1")?.token).toBe("fromA1")
    expect(tags.byMessage.get("A#3")?.token).toBe("fromA1")
  })

  it("reads a reverse-direction request as originator against answerer", () => {
    expect(recordedDialogTags(recording).byMessage.get("A#8")?.token).toBe("fromA1 · toA1")
  })

  it("draws the wire's tokens on the ladder, over the scenario's early ids", () => {
    expect(tokens(buildRecordingLayout(recording, { document: doc }))).toEqual([
      "fromA1",
      "fromA1 · toA1",
      "fromA1",
      "fromA1 · toA1",
      "fromA1 · toA2",
      "fromA1 · toA2",
      "fromA1 · toA2",
      "fromA1 · toA1"
    ])
  })

  it("names the wire's tokens in the legend, with the tag each stands for", () => {
    const layout = buildRecordingLayout(recording, { document: doc })
    expect(layout.dialogs.map((token) => token.token)).toEqual(["fromA1", "toA1", "toA2"])
    expect(layout.dialogs.find((token) => token.token === "toA2")?.value).toBe("t2")
  })

  it("falls back to the scenario's pair for a line that carries no parsed message", () => {
    const decoded = parseRecording({
      A: [
        line(1, "out", wire("INVITE sip:x@example.test SIP/2.0", "1 INVITE"), "s1"),
        JSON.stringify({ seq: 2, dir: "in", at_us: 2000, step: "s2", msg: { status: 180, reason: "Ringing", "cseq-method": "INVITE" } })
      ].join("\n")
    })
    expect(tokens(buildRecordingLayout(decoded, { document: doc }))).toEqual(["fromA1", "fromA1 · toA1"])
  })
})

describe("compare", () => {
  const doc = document([
    send("s1", "A", "INVITE"),
    expectResponse("s2", "A", 100, "Trying", { auto: true, check: "record" }),
    expectResponse("s3", "A", 200, "OK")
  ])
  const scenario = buildScenarioLayout(doc)

  it("pairs what ran, flags what did not, and flags what nobody asked for", () => {
    const recording = parseRecording({
      A: [
        JSON.stringify({ seq: 1, dir: "out", at_us: 0, raw: invite }),
        JSON.stringify({ seq: 2, dir: "in", at_us: 9000, msg: { status: 486, reason: "Busy Here", "cseq-method": "INVITE" } }),
        JSON.stringify({ seq: 3, dir: "in", at_us: 9500, msg: { status: 200, reason: "OK", "cseq-method": "INVITE" } })
      ].join("\n")
    })
    const result = compareLayouts(scenario, buildRecordingLayout(recording, { document: doc }))
    expect(result.counts).toEqual({ matched: 2, missing: 1, extra: 1 })
    expect(result.scenarioStatus["s2"]).toBe("missing")
    expect(result.scenarioStatus["s3"]).toBe("matched")
    expect(result.recordingStatus["A#2"]).toBe("extra")
  })

  it("uses the runner's own step attribution when the recording carries it", () => {
    const recording = parseRecording({
      A: [
        JSON.stringify({ seq: 1, dir: "out", at_us: 0, step: "s1", raw: invite }),
        JSON.stringify({ seq: 2, dir: "in", at_us: 500, step: "s3", msg: { status: 200, reason: "OK", "cseq-method": "INVITE" } })
      ].join("\n")
    })
    const result = compareLayouts(scenario, buildRecordingLayout(recording, { document: doc }))
    expect(result.scenarioStatus["s3"]).toBe("matched")
    expect(result.scenarioStatus["s2"]).toBe("missing")
    expect(result.counts.extra).toBe(0)
  })

  it("treats every scenario message as a message event", () => {
    expect(scenario.events.every(isMessageEvent)).toBe(true)
  })
})
