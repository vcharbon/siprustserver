import type { Body, Bundle, Flow, Flows, Pivot } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { capturedOf, confront, diffHeaders, recordOf, retransmissionProbes, scopeOfRaw, shapeProbes } from "../src/confront.js"
import type { MsgScope } from "../src/probe.js"
import { signature } from "../src/probe.js"
import { headersInOrderRaw } from "../src/wire.js"

const utf8 = new TextEncoder()

/** The layout the interpreter writes beside a recorded line that carries `body`, none where it carries none. */
const laidOut = (contentType: string, body: string | undefined) =>
  body === undefined || body.length === 0 ? {} : { body: { content_type: contentType, len: utf8.encode(body).length } }

const crlf = (lines: ReadonlyArray<string>): string => `${lines.join("\r\n")}\r\n\r\n`

const invite = crlf([
  "INVITE sip:+331@h.fr SIP/2.0",
  "Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK1",
  "From: <sip:+332@h.fr>;tag=a",
  "To: <sip:+331@h.fr>",
  "Call-ID: cid-1",
  "CSeq: 1 INVITE",
  "Allow: INVITE, ACK"
])

const ok200 = (callId: string, cseq: number, toTag: string): string =>
  crlf([
    "SIP/2.0 200 OK",
    `From: <sip:+332@h.fr>;tag=a`,
    `To: <sip:+331@h.fr>;tag=${toTag}`,
    `Call-ID: ${callId}`,
    `CSeq: ${cseq} INVITE`
  ])

const ack = (callId: string, cseq: number): string =>
  crlf(["ACK sip:+331@h.fr SIP/2.0", `Call-ID: ${callId}`, `CSeq: ${cseq} ACK`])

describe("scopeOfRaw", () => {
  it("an INVITE with no To tag is the initial INVITE", () => {
    expect(scopeOfRaw(invite)).toEqual({ kind: "initial-invite" })
  })
  it("a response carries its status and its transaction's method", () => {
    expect(scopeOfRaw(ok200("c", 1, "t"))).toEqual({
      kind: "response",
      status: 200,
      cseqMethod: "INVITE"
    })
  })
  it("a tagged request is in-dialog", () => {
    const bye = crlf(["BYE sip:x@h SIP/2.0", "To: <sip:a@h>;tag=t", "CSeq: 2 BYE"])
    expect(scopeOfRaw(bye)).toEqual({ kind: "request", method: "BYE", inDialog: true })
  })
})

describe("diffHeaders", () => {
  const scope: MsgScope = { kind: "initial-invite" }
  const none = new Map<string, ReadonlyArray<string>>()

  it("an equal message under the folds produces nothing", () => {
    const headers = headersInOrderRaw(invite)
    expect(diffHeaders(headers, headers, scope, none)).toEqual([])
  })

  it("a differing name produces one probe carrying both sides", () => {
    const captured = headersInOrderRaw(invite)
    const replayed = headersInOrderRaw(invite.replace("Allow: INVITE, ACK", "Allow: INVITE, BYE"))
    const probes = diffHeaders(captured, replayed, scope, none)
    expect(probes).toHaveLength(1)
    expect(probes[0]?.name).toBe("Allow")
    expect(signature(probes[0]!)).toBe("header:allow:initial-invite")
  })

  it("a captured-only header is a probe with an empty replayed side", () => {
    const captured = headersInOrderRaw(invite.replace("Allow: INVITE, ACK", "Reason: Q.850;cause=16"))
    const replayed = headersInOrderRaw(invite)
    const probes = diffHeaders(captured, replayed, scope, none)
    expect(probes.map((p) => [p.name, p.captured, p.replayed])).toEqual([
      ["Reason", ["Q.850;cause=16"], []],
      ["Allow", [], ["INVITE, ACK"]]
    ])
  })

  it("the driven set rides the probe, and no set at all states no input", () => {
    const captured = headersInOrderRaw(invite)
    const replayed = headersInOrderRaw(invite.replace("Allow: INVITE, ACK", "Allow: INVITE, BYE"))
    expect(diffHeaders(captured, replayed, scope, none)[0]?.driven).toBeUndefined()
    expect(diffHeaders(captured, replayed, scope, none, new Set(["allow"]))[0]?.driven).toBe(true)
    expect(diffHeaders(captured, replayed, scope, none, new Set())[0]?.driven).toBe(false)
  })

  it("inbound evidence rides the probe", () => {
    const inbound = new Map([["reason", ["Q.850;cause=16"]]])
    const captured = headersInOrderRaw(invite.replace("Allow: INVITE, ACK", "Reason: Q.850;cause=16"))
    const replayed = headersInOrderRaw(invite)
    const probe = diffHeaders(captured, replayed, scope, inbound).find((p) => p.name === "Reason")
    expect(probe?.inbound).toBe(true)
    expect(probe?.inboundValues).toEqual(["Q.850;cause=16"])
  })

  it("states whether both sides' messages are bodiless, and does not by default", () => {
    const captured = headersInOrderRaw(invite.replace("Allow: INVITE, ACK", "Content-Disposition: session"))
    const replayed = headersInOrderRaw(invite)
    expect(diffHeaders(captured, replayed, scope, none)[0]?.bodiless).toBe(false)
    expect(diffHeaders(captured, replayed, scope, none, undefined, "s1", true)[0]?.bodiless).toBe(true)
  })
})

const step = (id: string, over: Partial<Flow.Step> = {}): Flow.Step => ({
  id,
  leg: "A",
  op: "expect",
  msg: { "cseq-method": "INVITE", status: 486, headers: [], "headers-present": [] },
  delay: { ms: 0, from: "trigger" as Flow.Delay["from"], compressible: true, timer_linked: false },
  ...over
})

const verdictWith = (failures: Bundle.RunVerdict["failures"]): Bundle.RunVerdict => ({
  case: "c",
  lane: "fake",
  status: "failed",
  failures
})

describe("the relay input a reception was driven from", () => {
  // One relay, twice: the run sends on leg A, the system emits on leg B. The
  // captured pair carries P-Orig both ways; what differs is what the run drove.
  const capturedPrack = crlf([
    "PRACK sip:callee@platform SIP/2.0",
    "To: <sip:+331@h.fr>;tag=b",
    "CSeq: 2 PRACK",
    "P-Orig: sbc.113"
  ])
  const bare = crlf(["PRACK sip:b2bua@127.0.0.1 SIP/2.0", "To: <sip:+331@h.fr>;tag=b", "CSeq: 2 PRACK"])
  const dressed = crlf([
    "PRACK sip:b2bua@127.0.0.1 SIP/2.0",
    "To: <sip:+331@h.fr>;tag=b",
    "CSeq: 2 PRACK",
    "P-Orig: sbc.113"
  ])

  const pivot = (): Pivot.PivotV3 => ({
    pivot_version: 3,
    case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
    identities: [],
    calls: [],
    endpoints: [],
    actors: [],
    legs: [],
    flow: [step("s2", { leg: "B", msg: { method: "PRACK", headers: [], "headers-present": [] } })],
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  })

  const flows = (): Flows.FlowsDoc =>
    ({
      schema: 5,
      legs: [{ msgs: [{ raw: capturedPrack }] }]
    }) as unknown as Flows.FlowsDoc

  const recordings = (sent: string): ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>> =>
    new Map([
      ["A", [{ seq: 1, dir: "out", at_us: 1000, step: "s1", raw: sent }] as Array<Bundle.RecordedMessage>],
      ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s2", raw: bare }] as Array<Bundle.RecordedMessage>]
    ])

  const observed = { leg: 0, msg: 0, at_us: 0 }

  const run = (sent: string) => {
    const doc = pivot()
    const flow = [{ ...(doc.flow[0] as Flow.Step), observed }]
    return confront({ pivot: { ...doc, flow }, verdict: verdictWith([]), recordings: recordings(sent), captured: capturedOf(flows()) })
  }

  it("a bare input leaves the captured header undriven", () => {
    const probe = run(bare).probes.find((p) => p.probe.kind === "header" && p.probe.name === "P-Orig")
    expect(probe?.probe.kind === "header" && probe.probe.driven).toBe(false)
  })

  it("an input that carried the header drove it", () => {
    const probe = run(dressed).probes.find((p) => p.probe.kind === "header" && p.probe.name === "P-Orig")
    expect(probe?.probe.kind === "header" && probe.probe.driven).toBe(true)
  })

  it("a message the run drove no input for states no relay input at all", () => {
    const doc = pivot()
    const flow = [{ ...(doc.flow[0] as Flow.Step), observed }]
    const minted = new Map([
      ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s2", raw: bare }] as Array<Bundle.RecordedMessage>]
    ])
    const confronted = confront({ pivot: { ...doc, flow }, verdict: verdictWith([]), recordings: minted, captured: capturedOf(flows()) })
    const probe = confronted.probes.find((p) => p.probe.kind === "header" && p.probe.name === "P-Orig")
    expect(probe?.probe.kind === "header" && probe.probe.driven).toBeUndefined()
  })

  it("an input of another message is no input for this one", () => {
    const other = crlf(["BYE sip:b2bua@127.0.0.1 SIP/2.0", "To: <sip:+331@h.fr>;tag=b", "CSeq: 3 BYE"])
    const probe = run(other).probes.find((p) => p.probe.kind === "header" && p.probe.name === "P-Orig")
    expect(probe?.probe.kind === "header" && probe.probe.driven).toBeUndefined()
  })
})

describe("shapeProbes", () => {
  it("an unmatched final becomes a scoped status substitution", () => {
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s7",
          leg: "A",
          gated_on: { kind: "response", status: 486, cseq_method: "INVITE" },
          reason: "gated on response 486; 480 arrived",
          arrived: { kind: "response", status: 480, reason: "Temporarily Unavailable", cseq_method: "INVITE", cseq: 1 }
        }
      ]),
      new Map([["s7", step("s7")]]),
      new Map()
    )
    expect(probes).toHaveLength(1)
    expect(signature(probes[0]!.probe)).toBe("shape:status-substitution:486->480:initial-invite")
    expect(probes[0]?.step).toBe("s7")
  })

  it("an in-dialog substitution pins the answered transaction, not the initial final", () => {
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s9",
          leg: "A",
          gated_on: { kind: "response", status: 200 },
          reason: "gated on response 200; 481 arrived",
          arrived: { kind: "response", status: 481, reason: "Call/Transaction Does Not Exist", cseq_method: "BYE", cseq: 2 }
        }
      ]),
      new Map([["s9", step("s9", { in_dialog: true, msg: { "cseq-method": "BYE", status: 200, headers: [], "headers-present": [] } })]]),
      new Map()
    )
    expect(signature(probes[0]!.probe)).toBe("shape:status-substitution:200->481:response:481:BYE")
  })

  it("an unmatched request becomes a method substitution", () => {
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s4",
          leg: "B",
          gated_on: { kind: "request", method: "ACK" },
          reason: "gated on request ACK; BYE arrived",
          arrived: { kind: "request", method: "BYE", cseq: 2 }
        }
      ]),
      new Map(),
      new Map()
    )
    expect(signature(probes[0]!.probe)).toBe("shape:method-substitution:ACK->BYE")
  })

  it("an unmatched datagram of the other kind than the gate falls back to an extra message", () => {
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s4",
          leg: "B",
          gated_on: { kind: "response", status: 200, cseq_method: "INVITE" },
          reason: "gated on response 200; a BYE request arrived",
          arrived: { kind: "request", method: "BYE", cseq: 2 }
        }
      ]),
      new Map(),
      new Map()
    )
    expect(signature(probes[0]!.probe)).toBe("shape:extra-message:BYE")
  })

  it("a stray request the leg answered is a serviced stray", () => {
    const answer = crlf(["SIP/2.0 200 OK", "To: <sip:+331@h.fr>;tag=b", "Call-ID: cid-1", "CSeq: 2 BYE"])
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s4",
          leg: "B",
          gated_on: { kind: "response", status: 200, cseq_method: "INVITE" },
          reason: "gated on response 200; a BYE request arrived",
          arrived: { kind: "request", method: "BYE", cseq: 2 }
        }
      ]),
      new Map(),
      new Map([
        ["B", [{ seq: 1, dir: "out", at_us: 10, raw: answer }] as Array<Bundle.RecordedMessage>]
      ])
    )
    expect(signature(probes[0]!.probe)).toBe("shape:serviced-stray:BYE:auto-reacted")
  })

  it("an answer on another leg does not service the stray", () => {
    const answer = crlf(["SIP/2.0 200 OK", "To: <sip:+331@h.fr>;tag=b", "Call-ID: cid-1", "CSeq: 2 BYE"])
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s4",
          leg: "B",
          gated_on: { kind: "response", status: 200, cseq_method: "INVITE" },
          reason: "gated on response 200; a BYE request arrived",
          arrived: { kind: "request", method: "BYE", cseq: 2 }
        }
      ]),
      new Map(),
      new Map([
        ["A", [{ seq: 1, dir: "out", at_us: 10, raw: answer }] as Array<Bundle.RecordedMessage>]
      ])
    )
    expect(signature(probes[0]!.probe)).toBe("shape:extra-message:BYE")
  })

  it("an answer to another transaction on the leg does not service the stray", () => {
    const other = crlf(["SIP/2.0 200 OK", "To: <sip:+331@h.fr>;tag=b", "Call-ID: cid-1", "CSeq: 5 BYE"])
    const probes = shapeProbes(
      verdictWith([
        {
          failure: "unexpected-datagram",
          leg: "B",
          arrived: { kind: "request", method: "BYE", cseq: 2 }
        }
      ]),
      new Map(),
      new Map([
        ["B", [{ seq: 1, dir: "out", at_us: 10, raw: other }] as Array<Bundle.RecordedMessage>]
      ])
    )
    expect(signature(probes[0]!.probe)).toBe("shape:extra-message:BYE")
  })

  it("a datagram nothing expected is an extra message", () => {
    const probes = shapeProbes(
      verdictWith([
        { failure: "unexpected-datagram", leg: "B", arrived: { kind: "request", method: "OPTIONS", cseq: 9 } }
      ]),
      new Map(),
      new Map()
    )
    expect(signature(probes[0]!.probe)).toBe("shape:extra-message:OPTIONS")
  })

  it("an expectation nothing satisfied is a missing message", () => {
    const probes = shapeProbes(
      verdictWith([
        { failure: "expect-timed-out", step: "s7", leg: "A", gated_on: "response 486", within_ms: 32000 }
      ]),
      new Map(),
      new Map()
    )
    expect(signature(probes[0]!.probe)).toBe("shape:missing-message:response-486")
  })

  it("a passing verdict produces nothing", () => {
    expect(shapeProbes({ case: "c", lane: "fake", status: "ok" }, new Map(), new Map())).toEqual([])
  })
})

describe("retransmissionProbes", () => {
  const hold = { id: "d1", kind: "delayed-automatic", step: "s3", retransmits: 1 }
  const recording = (messages: ReadonlyArray<[Bundle.Dir, string, number, number | undefined]>) =>
    new Map([
      [
        "A",
        messages.map(([dir, raw, at_us, repeat], i) => ({
          seq: i + 1,
          dir,
          at_us,
          raw,
          ...(repeat === undefined ? {} : { repeat_of: repeat })
        })) satisfies ReadonlyArray<Bundle.RecordedMessage>
      ]
    ])

  it("a hold whose re-pass count matches the evidence produces nothing", () => {
    const recordings = recording([
      ["in", ok200("c1", 1, "t1"), 1000, undefined],
      ["in", ok200("c1", 1, "t1"), 2000, 1],
      ["out", ack("c1", 1), 3000, undefined]
    ])
    expect(retransmissionProbes([hold], recordings)).toEqual([])
  })

  it("a hold the capture evidences that provoked no re-pass is the probe", () => {
    const recordings = recording([
      ["in", ok200("c1", 1, "t1"), 1000, undefined],
      ["out", ack("c1", 1), 3000, undefined]
    ])
    const probes = retransmissionProbes([hold], recordings)
    expect(probes).toHaveLength(1)
    expect(signature(probes[0]!.probe)).toBe("shape:retransmission:INVITE:2xx")
    expect(probes[0]?.step).toBe("s3")
    const probe = probes[0]!.probe
    expect(probe.kind === "shape" && probe.shapeKind.shape === "retransmission" && probe.shapeKind.replayed).toBe(0)
  })

  it("a re-pass after the ACK counts for nothing", () => {
    const recordings = recording([
      ["in", ok200("c1", 1, "t1"), 1000, undefined],
      ["out", ack("c1", 1), 2000, undefined],
      ["in", ok200("c1", 1, "t1"), 3000, 1]
    ])
    const probes = retransmissionProbes([hold], recordings)
    const probe = probes[0]!.probe
    expect(probe.kind === "shape" && probe.shapeKind.shape === "retransmission" && probe.shapeKind.replayed).toBe(0)
  })

  it("a forked second 2xx is a distinct final, not a re-pass", () => {
    const recordings = recording([
      ["in", ok200("c1", 1, "t1"), 1000, undefined],
      ["in", ok200("c1", 1, "t2"), 1500, undefined],
      ["out", ack("c1", 1), 3000, undefined]
    ])
    const probes = retransmissionProbes([{ ...hold, retransmits: 0 }], recordings)
    expect(probes).toEqual([])
  })

  it("no declared hold, no probe — agreement produces nothing", () => {
    expect(retransmissionProbes([], new Map())).toEqual([])
  })
})

describe("an expected body held against the one received", () => {
  const XML = '<?xml version="1.0" encoding="utf-8"?>\r\n<request><play><prompt><audio url="a.wav"/></prompt></play></request>'
  const REF = "resources/uas1_r0_0.xml"

  const info = (body: string | undefined): string =>
    crlf([
      "INFO sip:callee@127.0.0.1 SIP/2.0",
      "To: <sip:+331@h.fr>;tag=b",
      "CSeq: 2 INFO",
      ...(body === undefined ? [] : ["Content-Type: application/example+xml;charset=utf-8"]),
      `Content-Length: ${body?.length ?? 0}`
    ]) + (body ?? "")

  const expecting = (body: Body.Body, op: Flow.Step["op"] = "expect"): Pivot.PivotV3 => ({
    pivot_version: 3,
    case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
    identities: [],
    calls: [],
    endpoints: [],
    actors: [],
    legs: [],
    flow: [step("s9", { leg: "B", op, in_dialog: true, check: "record", msg: { method: "INFO", body } })],
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  })

  const resource = (compare?: Body.BodyCompare): Body.ResourceBody => ({
    ref: REF,
    mode: "frozen",
    "content-type": "application/example+xml;charset=utf-8",
    ...(compare === undefined ? {} : { compare })
  })

  const run = (body: Body.Body, received: string | undefined, resources = new Map([[REF, utf8.encode(XML)]])) =>
    confront({
      pivot: expecting(body),
      verdict: verdictWith([]),
      recordings: new Map([
        ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s9", raw: info(received), ...laidOut("application/example+xml", received) }] as Array<Bundle.RecordedMessage>]
      ]),
      resources
    }).probes.filter((p) => p.probe.kind === "body")

  it("a body that differs is one probe carrying both texts, under the media type and scope", () => {
    const wrapped = XML.replace("<request>", "<request><wrapper>").replace("</request>", "</wrapper></request>")
    const probes = run(resource(), wrapped)
    expect(probes).toHaveLength(1)
    const probe = probes[0]!.probe
    expect(probe.kind === "body" && probe).toMatchObject({
      step: "s9",
      mediaType: "application/example+xml",
      compare: "exact",
      captured: [XML],
      replayed: [wrapped]
    })
    expect(signature(probe)).toBe("body:application/example+xml:request:INFO:in-dialog")
  })

  it("an equal body produces nothing", () => {
    expect(run(resource(), XML)).toEqual([])
  })

  it("`xml` tolerates a declaration and whitespace between tags, and nothing else", () => {
    const reflowed = "<request>\r\n  <play>\r\n    <prompt><audio url=\"a.wav\"/></prompt>\r\n  </play>\r\n</request>\r\n"
    expect(run(resource("xml"), reflowed)).toEqual([])
    expect(run(resource(), reflowed)).toHaveLength(1)
    const retargeted = reflowed.replace("a.wav", "b.wav")
    const probes = run(resource("xml"), retargeted)
    expect(probes).toHaveLength(1)
    // The record keeps both sides as the wire carried them: the fold decides, it does not edit.
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe.replayed).toEqual([retargeted])
  })

  it("a reception with no body at all is confronted, as the empty text", () => {
    const probes = run(resource(), undefined)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe.replayed).toEqual([""])
  })

  it("a resource with no `mode` is text, and is held against the file", () => {
    const { mode: _, ...modeless } = resource()
    expect(run(modeless, XML)).toEqual([])
    const probes = run(modeless, XML.replace("a.wav", "b.wav"))
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe.compare).toBe("exact")
  })

  it("an expectation stating no content type is keyed by the reception's own", () => {
    const { "content-type": _, ...untyped } = resource()
    const probes = run(untyped, XML.replace("a.wav", "b.wav"))
    expect(probes).toHaveLength(1)
    expect(signature(probes[0]!.probe)).toBe("body:application/example+xml:request:INFO:in-dialog")
  })

  it("a shape or a send is not read here", () => {
    expect(run({ mode: "absent" }, XML)).toEqual([])
    const sent = confront({
      pivot: expecting(resource(), "send"),
      verdict: verdictWith([]),
      recordings: new Map([
        ["B", [{ seq: 1, dir: "out", at_us: 1200, step: "s9", raw: info("not the file") }] as Array<Bundle.RecordedMessage>]
      ]),
      resources: new Map()
    })
    expect(sent.probes.filter((p) => p.probe.kind === "body")).toEqual([])
  })

  describe("compared as a session description", () => {
    const SDP_REF = "resources/uas1_r0_0.sdp"
    const OFFER =
      "v=0\r\no=- 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\n" +
      "m=audio 6000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\na=sendrecv\r\n"
    const described: Body.ResourceBody = { ref: SDP_REF, rewrite: ["c=addr", "m=port"], compare: "sdp" }
    const sdpInfo = (body: string | undefined): string =>
      crlf([
        "INFO sip:callee@127.0.0.1 SIP/2.0",
        "To: <sip:+331@h.fr>;tag=b",
        "CSeq: 2 INFO",
        ...(body === undefined ? [] : ["Content-Type: application/sdp"]),
        `Content-Length: ${body?.length ?? 0}`
      ]) + (body ?? "")
    const runSdp = (received: string | undefined, media?: Bundle.MediaMode) =>
      confront({
        pivot: expecting(described),
        verdict: verdictWith([]),
        recordings: new Map([
          ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s9", raw: sdpInfo(received), ...laidOut("application/sdp", received) }] as Array<Bundle.RecordedMessage>]
        ]),
        resources: new Map([[SDP_REF, utf8.encode(OFFER)]]),
        ...(media === undefined ? {} : { media })
      }).probes.filter((p) => p.probe.kind === "body")

    it("one probe per differing line key, signed by section and key, the media type kept as the name", () => {
      const probes = runSdp(OFFER.replace("a=ptime:20", "a=ptime:30"))
      expect(probes).toHaveLength(1)
      const probe = probes[0]!.probe
      expect(probe.kind === "body" && probe).toMatchObject({
        step: "s9",
        mediaType: "application/sdp",
        compare: "sdp",
        captured: ["a=ptime:20"],
        replayed: ["a=ptime:30"],
        sdp: { section: "m0", line: "a=ptime" }
      })
      expect(signature(probe)).toBe("body:sdp:m0:a=ptime:request:INFO:in-dialog")
    })

    it("carries one element per verbatim line of the key, the way a header record carries one per value", () => {
      const probes = runSdp(OFFER.replace("a=rtpmap:8 PCMA/8000", "a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000"))
      expect(probes).toHaveLength(1)
      const at = probes[0]!
      expect(at.probe.kind === "body" && at.probe).toMatchObject({
        captured: ["a=rtpmap:8 PCMA/8000"],
        replayed: ["a=rtpmap:8 PCMA/8000", "a=rtpmap:101 telephone-event/8000"]
      })
      const record = recordOf({ lane: "l", capture: "c", case: "k", run: 0 }, at, { class: "unlisted", rule: "", ticket: "" })
      expect(record.captured).toEqual(["a=rtpmap:8 PCMA/8000"])
      expect(record.replayed).toEqual(["a=rtpmap:8 PCMA/8000", "a=rtpmap:101 telephone-event/8000"])
    })

    it("equal under the mask produces nothing; the mask reads the tokens only where the run rebooked", () => {
      const rebooked = OFFER.replace("c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2").replace("m=audio 6000", "m=audio 40000")
      expect(runSdp(OFFER.replace("o=- 1 2", "o=- 9 9"))).toEqual([])
      expect(runSdp(rebooked)).toEqual([])
      expect(runSdp(rebooked, "rebooked")).toEqual([])
      expect(runSdp(rebooked, "verbatim").map((p) => signature(p.probe))).toEqual([
        "body:sdp:session:c=:request:INFO:in-dialog",
        "body:sdp:m0:m=:request:INFO:in-dialog"
      ])
    })

    it("on a verbatim run the same structure in other bytes is one `document:bytes` row", () => {
      const bareLf = OFFER.replace(/\r\n/g, "\n")
      expect(runSdp(bareLf, "rebooked")).toEqual([])
      const probes = runSdp(bareLf, "verbatim")
      expect(probes.map((p) => signature(p.probe))).toEqual(["body:sdp:document:bytes:request:INFO:in-dialog"])
      expect(probes[0]!.probe.kind === "body" && probes[0]!.probe).toMatchObject({ captured: [OFFER], replayed: [bareLf] })
    })

    it("a body the system dropped whole is one `document` row, the captured text against the empty one", () => {
      const probes = runSdp(undefined)
      expect(probes.map((p) => signature(p.probe))).toEqual(["body:sdp:document:sdp:request:INFO:in-dialog"])
      expect(probes[0]!.probe.kind === "body" && probes[0]!.probe).toMatchObject({ captured: [OFFER], replayed: [""] })
    })
  })

  it("a status-substituted step keeps the substitution alone: its body probe is dropped with its header probes", () => {
    const answer = (status: number, body: string): string =>
      crlf([
        `SIP/2.0 ${status} Reply`,
        "From: <sip:+332@h.fr>;tag=a",
        "To: <sip:+331@h.fr>;tag=b",
        "Call-ID: cid-1",
        "CSeq: 2 INFO",
        "Content-Type: application/example+xml",
        `Content-Length: ${body.length}`
      ]) + body
    const flow = [
      step("s9", {
        leg: "B",
        in_dialog: true,
        check: "record",
        observed: { leg: 0, msg: 0, at_us: 0 },
        msg: { "cseq-method": "INFO", status: 200, body: resource() }
      })
    ]
    const confronted = confront({
      pivot: { ...expecting(resource()), flow },
      verdict: verdictWith([
        {
          failure: "unmatched-datagram",
          step: "s9",
          leg: "B",
          gated_on: { kind: "response", status: 200, cseq_method: "INFO" },
          reason: "gated on response 200; 481 arrived",
          arrived: { kind: "response", status: 481, reason: "Reply", cseq_method: "INFO", cseq: 2 }
        }
      ]),
      recordings: new Map([
        ["B", [{ seq: 1, dir: "in", at_us: 1200, step: "s9", raw: answer(481, "<other/>"), ...laidOut("application/example+xml", "<other/>") }] as Array<Bundle.RecordedMessage>]
      ]),
      captured: capturedOf({ schema: 5, legs: [{ msgs: [{ raw: answer(200, XML) }] }] } as unknown as Flows.FlowsDoc),
      resources: new Map([[REF, utf8.encode(XML)]])
    })
    expect(confronted.probes.map((p) => signature(p.probe))).toEqual([
      "shape:status-substitution:200->481:response:481:INFO"
    ])
  })

  it("a resource the driver did not supply is the driver's error, named by ref", () => {
    expect(() => run(resource(), XML, new Map())).toThrow(REF)
  })
})

describe("recordOf", () => {
  it("a body probe lands its media type as the name and one text a side", () => {
    const record = recordOf(
      { lane: "fake", capture: "cap.pcap.gz", case: "cap-case1", run: 0 },
      {
        step: "s9",
        probe: {
          kind: "body",
          step: "s9",
          mediaType: "application/example+xml",
          scope: { kind: "request", method: "INFO", inDialog: true },
          compare: "exact",
          captured: ["<a/>"],
          replayed: ["<b><a/></b>"]
        }
      },
      { class: "unlisted", rule: "", ticket: "" }
    )
    expect(record).toEqual({
      lane: "fake",
      capture: "cap.pcap.gz",
      case: "cap-case1",
      run: 0,
      step: "s9",
      kind: "body",
      signature: "body:application/example+xml:request:INFO:in-dialog",
      name: "application/example+xml",
      scope: "request:INFO:in-dialog",
      captured: ["<a/>"],
      replayed: ["<b><a/></b>"],
      inbound: false,
      added: [],
      removed: [],
      class: "unlisted",
      rule: "",
      ticket: ""
    })
  })

  it("fills every key of the flat record", () => {
    const record = recordOf(
      { lane: "fake", capture: "cap.pcap.gz", case: "cap-case1", run: 0 },
      {
        step: "s2",
        probe: {
          kind: "header",
          step: "s2",
          name: "Allow",
          scope: { kind: "initial-invite" },
          captured: ["INVITE, ACK"],
          replayed: ["INVITE, ACK, BYE"],
          inbound: true,
          inboundValues: ["INVITE, ACK"],
          driven: true,
          bodiless: false
        }
      },
      { class: "accepted", rule: "capability-set-added-by-stack", ticket: "" }
    )
    expect(record).toEqual({
      lane: "fake",
      capture: "cap.pcap.gz",
      case: "cap-case1",
      run: 0,
      step: "s2",
      kind: "header",
      signature: "header:allow:initial-invite",
      name: "Allow",
      scope: "initial-invite",
      captured: ["INVITE, ACK"],
      replayed: ["INVITE, ACK, BYE"],
      inbound: true,
      added: ["BYE"],
      removed: [],
      class: "accepted",
      rule: "capability-set-added-by-stack",
      ticket: ""
    })
  })
})

describe("the document's own shape", () => {
  const doc = (flow: ReadonlyArray<Flow.Step>): Pivot.PivotV3 => ({
    pivot_version: 3,
    case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
    identities: [],
    calls: [],
    endpoints: [],
    actors: [],
    legs: [],
    flow,
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  })

  const sendFinal = (id: string, leg: string, status: number): Flow.Step =>
    step(id, { leg, op: "send", msg: { "cseq-method": "INVITE", status, headers: [], "headers-present": [] } })

  const expectAck = (id: string, leg: string): Flow.Step =>
    step(id, { leg, op: "expect", auto: true, msg: { method: "ACK", headers: [], "headers-present": [] } })

  const read = (flow: ReadonlyArray<Flow.Step>) =>
    confront({ pivot: doc(flow), verdict: verdictWith([]), recordings: new Map() }).context.document

  it("names the final a leg ENDS on, with no ACK expect after it", () => {
    const finals = read([sendFinal("s5", "B", 500), expectAck("s6", "B"), sendFinal("s19", "D", 487)])
      .unackedFinals
    expect(finals).toEqual([{ step: "s19", leg: "D", status: 487, legTail: true, repeated: false }])
  })

  it("a final the flow goes on past is named, and marked as no leg tail", () => {
    const finals = read([sendFinal("s5", "B", 500), step("s7", { leg: "B", op: "expect" })]).unackedFinals
    expect(finals).toEqual([{ step: "s5", leg: "B", status: 500, legTail: false, repeated: false }])
  })

  it("a re-INVITE's ACK never stands in for the transaction before it", () => {
    const finals = read([
      sendFinal("s3", "B", 200),
      sendFinal("s9", "B", 200),
      expectAck("s10", "B")
    ]).unackedFinals
    expect(finals).toEqual([{ step: "s3", leg: "B", status: 200, legTail: false, repeated: false }])
  })

  it("a provisional is no final, and another leg's ACK is not this leg's", () => {
    const provisional = step("s2", {
      leg: "B",
      op: "send",
      msg: { "cseq-method": "INVITE", status: 180, headers: [], "headers-present": [] }
    })
    expect(read([provisional]).unackedFinals).toEqual([])
    expect(read([sendFinal("s5", "B", 500), expectAck("s6", "C")]).unackedFinals).toEqual([
      { step: "s5", leg: "B", status: 500, legTail: true, repeated: false }
    ])
  })

  it("marks a final the document states a retransmit ladder for", () => {
    const laddered = { ...sendFinal("s5", "B", 200), retransmits: 2 }
    expect(read([laddered]).unackedFinals).toEqual([
      { step: "s5", leg: "B", status: 200, legTail: true, repeated: true }
    ])
  })
})
