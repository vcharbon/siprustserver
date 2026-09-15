/**
 * The flows-document mirror. Decode-only: nothing re-emits a flows document, so
 * there is no byte discipline to hold, and the Rust struct does not deny unknown
 * fields — an emitter that grows a field must not break a reader that has not
 * caught up.
 *
 * `anon-capture.flows.json` is the smallest anonymized real capture the corpus
 * holds, committed here so the sweep runs with no corpus checkout. It covers the
 * `text` payload arm and two evidence kinds; the other arms and kinds are pinned
 * by the inline fixtures below, since no small real capture carries them.
 */
import { describe, expect, it } from "vitest"
import {
  decodeFlowsSync,
  EMIT_SCHEMA_VERSION,
  groupsForLegs,
  headerValues,
  isInvite,
  isMethod,
  type Msg,
  payloadOf,
  requireHeaders,
  samePair,
  userOf
} from "../src/flows.js"
import { LOCAL_FIXTURES, read } from "./fixtures.js"

const doc = decodeFlowsSync(JSON.parse(read(LOCAL_FIXTURES, "anon-capture.flows.json")) as unknown)

describe("a real anonymized capture", () => {
  it("decodes at the schema version this contract models", () => {
    expect(doc.schema).toBe(EMIT_SCHEMA_VERSION)
    expect(doc.legs.length).toBe(4)
    expect(doc.groups.length).toBe(2)
  })

  it("carries the schema-4 legacy fields as explicit nulls, never as absences", () => {
    for (const leg of doc.legs) {
      expect(leg.invite === null || typeof leg.invite === "object").toBe(true)
      expect(leg.final_status === null || typeof leg.final_status === "number").toBe(true)
      expect(leg.terminated_by === null || typeof leg.terminated_by === "string").toBe(true)
      for (const msg of leg.msgs) {
        expect(msg.summary.from.tag === null || typeof msg.summary.from.tag === "string").toBe(true)
        for (const via of msg.via ?? []) expect(via.branch === null || typeof via.branch === "string").toBe(true)
      }
    }
  })

  it("reads the subscriber a URI names, digits first", () => {
    const first = doc.legs[0].msgs[0]
    expect(userOf(first.identities.from)).toBe("33000900001")
    expect(userOf(undefined)).toBeUndefined()
  })

  it("answers which groups a set of vantage legs is a case of", () => {
    expect(groupsForLegs(doc, [0])).toEqual([0])
    expect(groupsForLegs(doc, [999])).toEqual([])
  })

  it("classifies requests by method", () => {
    const invites = doc.legs.flatMap((leg) => leg.msgs.filter(isInvite))
    expect(invites.length).toBeGreaterThan(0)
    expect(isMethod(invites[0], "invite")).toBe(true)
    expect(isMethod(invites[0], "BYE")).toBe(false)
  })
})

const REQUEST = {
  kind: "request",
  method: "INVITE",
  uri: "sip:b@h",
  cseq: { seq: 1, method: "INVITE" },
  from: { uri: "sip:a@h", tag: "t1" },
  to: { uri: "sip:b@h", tag: null }
}

const base = {
  ts_us: 1,
  src: "1.1.1.1:5060",
  dst: "2.2.2.2:5060",
  hop: 0,
  retx: false,
  probe: 0,
  summary: REQUEST
}

const wrap = (msg: unknown, groups: Array<unknown> = []) => ({
  schema: 5,
  emit_headers: ["P-Charging-Vector"],
  decode_stats: {
    records: 0,
    non_ip: 0,
    non_udp: 0,
    snap_truncated: 0,
    datagrams: 0,
    fragments: 0,
    reassembled: 0,
    frag_dropped: 0,
    tail_truncated: 0
  },
  flow_stats: { sip_messages: 0, capture_dups: 0, parse_failed: 0, non_sip: 0 },
  legs: [
    {
      call_id: "c",
      hops: [{ a: "1.1.1.1:5060", b: "2.2.2.2:5060" }],
      invite: null,
      final_status: null,
      saw_180: false,
      terminated_by: null,
      tokens: [],
      msgs: [msg]
    }
  ],
  groups
})

describe("the three payload arms", () => {
  it("reads `raw` as the whole-UTF-8 form", () => {
    const decoded = decodeFlowsSync(wrap({ ...base, raw: "INVITE sip:b@h SIP/2.0\r\n\r\n" }))
    expect(payloadOf(decoded.legs[0].msgs[0])).toEqual({ _tag: "text", raw: "INVITE sip:b@h SIP/2.0\r\n\r\n" })
  })

  it("reads `head` + `body_b64` as the split form", () => {
    const decoded = decodeFlowsSync(wrap({ ...base, head: "INVITE sip:b@h SIP/2.0\r\n\r\n", body_b64: "AAEC" }))
    expect(payloadOf(decoded.legs[0].msgs[0])).toEqual({
      _tag: "head-body",
      head: "INVITE sip:b@h SIP/2.0\r\n\r\n",
      body_b64: "AAEC"
    })
  })

  it("reads `raw_b64` as the opaque form", () => {
    const decoded = decodeFlowsSync(wrap({ ...base, raw_b64: "AAEC" }))
    expect(payloadOf(decoded.legs[0].msgs[0])).toEqual({ _tag: "opaque", raw_b64: "AAEC" })
  })

  it("refuses a message that carries no payload at all", () => {
    expect(() => decodeFlowsSync(wrap(base))).toThrow()
  })

  it("defaults the identities block a schema-4 document never wrote", () => {
    const decoded = decodeFlowsSync(wrap({ ...base, raw: "X" }))
    expect(decoded.legs[0].msgs[0].identities).toEqual({
      from: { uri: "", user: null, digits: null },
      to: { uri: "", user: null, digits: null }
    })
  })
})

describe("every evidence kind", () => {
  const kinds = [
    { kind: "shared_token", strategy: 0, token: "t", legs: [0, 1] },
    { kind: "shared_header_param", strategy: 1, header: "P-Charging-Vector", param: "icid-value", token: "t", legs: [0, 1] },
    {
      kind: "derived_call_id",
      strategy: 2,
      legs: [0, 1],
      prefix: "1-",
      as_socket: "1.1.1.1:5060",
      peer_socket: "2.2.2.2:5060",
      shared_hop: true,
      dt_us: 10
    },
    { kind: "identity_adjacency", strategy: 3, legs: [0, 1], shared_host: "h", dt_us: 10 }
  ]

  it.each(kinds)("decodes $kind", (evidence) => {
    const decoded = decodeFlowsSync(wrap({ ...base, raw: "X" }, [{ legs: [0], evidence: [evidence], t0_us: 1 }]))
    expect(decoded.groups[0].evidence[0].kind).toBe(evidence.kind)
  })

  it("refuses a kind no strategy produces", () => {
    expect(() =>
      decodeFlowsSync(wrap({ ...base, raw: "X" }, [{ legs: [0], evidence: [{ kind: "vibes", legs: [0] }] }]))
    ).toThrow()
  })

  it("defaults a group's t0_us, which a schema-4 document never wrote", () => {
    const decoded = decodeFlowsSync(wrap({ ...base, raw: "X" }, [{ legs: [0], evidence: [] }]))
    expect(decoded.groups[0].t0_us).toBe(0)
  })
})

describe("the header allow-list", () => {
  const msg = (headers: Array<unknown>) => decodeFlowsSync(wrap({ ...base, raw: "X", headers })).legs[0].msgs[0]

  it("returns every instance of a header, wire order preserved", () => {
    const m = msg([
      { name: "P-Charging-Vector", value: "icid-value=a" },
      { name: "Allow", value: "INVITE" },
      { name: "P-Charging-Vector", wire: "p-charging-vector", value: "icid-value=b" }
    ])
    expect(headerValues(m, "p-charging-vector")).toEqual(["icid-value=a", "icid-value=b"])
    expect(headerValues(m, "Missing")).toEqual([])
  })

  it("refuses a document that does not project a header a rule names, and says how to fix it", () => {
    expect(() => requireHeaders(doc, ["P-Charging-Vector"])).toThrow(/--emit-headers P-Charging-Vector/)
    expect(() => requireHeaders(decodeFlowsSync(wrap({ ...base, raw: "X" })), ["P-Charging-Vector"])).not.toThrow()
  })
})

describe("hops", () => {
  it("compares a socket pair without regard to the direction first seen", () => {
    expect(samePair({ a: "x", b: "y" }, { a: "y", b: "x" })).toBe(true)
    expect(samePair({ a: "x", b: "y" }, { a: "x", b: "z" })).toBe(false)
  })
})

describe("flow stats", () => {
  it("reads the probes the extractor rebased, and their absence as none", () => {
    const doc = wrap({ ...base, raw: "X" }) as { flow_stats: Record<string, unknown> }
    expect(decodeFlowsSync(doc).flow_stats.aligned_probes).toBeUndefined()
    doc.flow_stats = {
      ...doc.flow_stats,
      aligned_probes: [{ probe: 1, reference: 2, from_us: 0, offset_us: 248_000, pairs: 12 }]
    }
    expect(decodeFlowsSync(doc).flow_stats.aligned_probes).toEqual([
      { probe: 1, reference: 2, from_us: 0, offset_us: 248_000, pairs: 12 }
    ])
  })
})

describe("a leniently decoded document", () => {
  it("carries an unknown field through rather than refusing it", () => {
    const grown: Record<string, unknown> = { ...(wrap({ ...base, raw: "X" }) as object) }
    grown.future_field = 1
    const decoded: { legs: ReadonlyArray<{ msgs: ReadonlyArray<Msg> }> } = decodeFlowsSync(grown)
    expect(decoded.legs[0].msgs.length).toBe(1)
  })
})
