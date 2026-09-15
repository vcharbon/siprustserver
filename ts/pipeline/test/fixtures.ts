/**
 * Synthetic schema-5 flows documents, small enough to read.
 *
 * The builders emit the same fields `sipflow --json` does — the verbatim `raw`
 * datagram included — because the pipeline reads the datagram, not the summary.
 * A fixture that only filled the summary would pass tests the real extractor's
 * output fails.
 */
import type { Flows } from "@sip/contracts"
import { neverDerives, type CallIdDerivation } from "../src/derivation.js"
import { Plan, type PlanDoc } from "../src/plan.js"
import { SutSet } from "../src/sut.js"

const CRLF = "\r\n"

const CALLER = "10.0.0.9:5060"
const SUT = "10.0.0.1:5060"
const CALLEE = "10.0.0.2:5060"
const OTHER = "10.0.0.3:5060"

export const SOCKETS = { caller: CALLER, sut: SUT, callee: CALLEE, other: OTHER } as const

/** The platform, alone: the address set every fixture below is cut against. */
export const SUT_ADDRESSES = ["10.0.0.1"]

export const sutSet = (): SutSet => new SutSet(SUT_ADDRESSES)

export const CALLER_URI = "sip:+33600000001@10.0.0.9"
export const CALLEE_URI = "sip:+33600000004@10.0.0.1"

/** A plan with one country, so a `+33` number classifies and anything else does not. */
export const PLAN_DOC: PlanDoc = {
  default_cc: "33",
  countries: [{ cc: "33", nsn_len: 9, national_prefix: "0" }],
  trunks: [],
  private_len: { min: 4, max: 6 },
  max_composed_prefix: 8,
  fake_prefix: "0411"
}

export const plan = (): Plan => new Plan(PLAN_DOC)

export const CALLER_CALL_ID = "caller-leg-call-id"

/**
 * The b-leg's Call-ID is DERIVED from the a-leg's, which is how a B2BUA that
 * re-originates states the relation on the wire. It is the only correlation the
 * cut reads, and {@link derivesOnePrefix} is the reading that sees it.
 */
export const CALLEE_CALL_ID = `1-${CALLER_CALL_ID}`

/** The convention these fixtures were minted under: `1-<base>`. */
export const derivesOnePrefix: CallIdDerivation = (base, derived) => derived === `1-${base}`

/** The neutral reading, re-exported so a test can state which one it used. */
export const derivesNothing = neverDerives

const datagram = (start: string, headers: ReadonlyArray<string>): string =>
  [start, ...headers, "", ""].join(CRLF)

const identityOf = (uri: string): Flows.Identity => {
  const at = uri.indexOf("@")
  const colon = uri.indexOf(":")
  const user = at < 0 ? null : uri.slice(colon + 1, at)
  return { uri, user, digits: user === null ? null : user.replace(/[^0-9]/g, "") }
}

interface Common {
  readonly callId: string
  readonly seq: number
  readonly src: string
  readonly dst: string
  readonly ts_ms: number
  /** Which hop of the leg carries it. Zero — one hop — unless stated. */
  readonly hop?: number
}

export const request = (
  o: Common & {
    readonly method: string
    readonly toTag?: string
    readonly fromTag?: string
    /** The From URI, where the request travels back down the leg it did not open. */
    readonly fromUri?: string
    readonly ruri?: string
    /** Extra header lines, verbatim, after the mandatory set. */
    readonly headers?: ReadonlyArray<string>
    /** The Via branch, where the message is not the first of its transaction. */
    readonly branch?: string
    readonly body?: { readonly contentType: string; readonly text: string }
  }
): Flows.Msg => {
  const ruri = o.ruri ?? CALLEE_URI
  const fromUri = o.fromUri ?? CALLER_URI
  const fromTag = o.fromTag ?? `from-${o.callId}`
  return {
    ts_us: o.ts_ms * 1000,
    src: o.src,
    dst: o.dst,
    hop: o.hop ?? 0,
    retx: false,
    probe: 0,
    raw:
      datagram(`${o.method} ${ruri} SIP/2.0`, [
        `Via: SIP/2.0/UDP ${o.src};branch=${o.branch ?? `z9hG4bK-${o.callId}-${o.seq}-${o.method}`}`,
        `From: <${fromUri}>;tag=${fromTag}`,
        `To: <${ruri}>${o.toTag === undefined ? "" : `;tag=${o.toTag}`}`,
        `Call-ID: ${o.callId}`,
        `CSeq: ${o.seq} ${o.method}`,
        "Max-Forwards: 70",
        ...(o.headers ?? []),
        ...(o.body === undefined ? [] : [`Content-Type: ${o.body.contentType}`]),
        `Content-Length: ${o.body?.text.length ?? 0}`
      ]) + (o.body?.text ?? ""),
    ...(o.body === undefined
      ? {}
      : { body: { content_type: o.body.contentType, len: o.body.text.length } }),
    identities: {
      from: identityOf(fromUri),
      to: identityOf(ruri),
      ruri: identityOf(ruri)
    },
    summary: {
      kind: "request",
      method: o.method,
      uri: ruri,
      cseq: { seq: o.seq, method: o.method },
      from: { uri: fromUri, tag: fromTag },
      to: { uri: ruri, tag: o.toTag ?? null }
    }
  }
}

export const response = (
  o: Common & {
    readonly status: number
    readonly reason: string
    readonly cseqMethod: string
    readonly toTag?: string
    readonly fromTag?: string
    /** The dialog's two URIs, where the request answered travelled back down the leg. */
    readonly fromUri?: string
    readonly toUri?: string
    readonly headers?: ReadonlyArray<string>
    /** An SDP body, for the steps that store one. */
    readonly sdp?: string
  }
): Flows.Msg => {
  const fromUri = o.fromUri ?? CALLER_URI
  const toUri = o.toUri ?? CALLEE_URI
  const fromTag = o.fromTag ?? `from-${o.callId}`
  return {
    ts_us: o.ts_ms * 1000,
    src: o.src,
    dst: o.dst,
    hop: o.hop ?? 0,
    retx: false,
    probe: 0,
    raw: datagram(`SIP/2.0 ${o.status} ${o.reason}`, [
      `Via: SIP/2.0/UDP ${o.dst};branch=z9hG4bK-${o.callId}-${o.seq}-${o.cseqMethod}`,
      `From: <${fromUri}>;tag=${fromTag}`,
      `To: <${toUri}>${o.toTag === undefined ? "" : `;tag=${o.toTag}`}`,
      `Call-ID: ${o.callId}`,
      `CSeq: ${o.seq} ${o.cseqMethod}`,
      ...(o.headers ?? []),
      ...(o.sdp === undefined ? [] : ["Content-Type: application/sdp"]),
      `Content-Length: ${o.sdp?.length ?? 0}`
    ]) + (o.sdp ?? ""),
    ...(o.sdp === undefined
      ? {}
      : { body: { content_type: "application/sdp", len: o.sdp.length } }),
    identities: { from: identityOf(fromUri), to: identityOf(toUri) },
    summary: {
      kind: "response",
      status: o.status,
      reason: o.reason,
      cseq: { seq: o.seq, method: o.cseqMethod },
      from: { uri: fromUri, tag: fromTag },
      to: { uri: toUri, tag: o.toTag ?? null }
    }
  }
}

export const leg = (
  callId: string,
  hops: ReadonlyArray<Flows.Hop>,
  msgs: ReadonlyArray<Flows.Msg>
): Flows.Leg => ({
  call_id: callId,
  hops,
  invite: { ruri: CALLEE_URI, from_uri: CALLER_URI, to_uri: CALLEE_URI, cseq: 1 },
  final_status: null,
  saw_180: true,
  terminated_by: null,
  tokens: [],
  msgs
})

/** One hop, the common case. */
export const oneHop = (a: string, b: string): ReadonlyArray<Flows.Hop> => [{ a, b }]

export const doc = (
  legs: ReadonlyArray<Flows.Leg>,
  groups: ReadonlyArray<Partial<Flows.Group> & { readonly legs: ReadonlyArray<number> }>
): Flows.FlowsDoc => ({
  schema: 5,
  emit_headers: [],
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
  legs,
  groups: groups.map((g) => ({ evidence: [], t0_us: 0, ...g }))
})

/**
 * The cancel race across a B2BUA: the caller CANCELs, the platform relays it,
 * and the far callee answers 200 to the INVITE anyway 40 ms later. Leg 0 is the
 * caller's dialog, leg 1 the callee's, and the b-leg Call-ID derives from the
 * a-leg's.
 */
export const cancelRaceFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "CANCEL", src: CALLER, dst: SUT, ts_ms: 1_000 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: SUT, dst: CALLER, ts_ms: 1_010, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_100, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_105, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 190, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "CANCEL", src: SUT, dst: CALLEE, ts_ms: 1_020 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: CALLEE, dst: SUT, ts_ms: 1_025, toTag: "callee-tag" }),
        // The violation: a 200 to the INVITE the callee had already taken the
        // CANCEL for (RFC 3261 §9.2).
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_060, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_065, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 1_070, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 1_075, toTag: "callee-tag" })
      ])
    ],
    [
      {
        legs: [0, 1],
        evidence: [
          {
            kind: "derived_call_id",
            strategy: 1,
            legs: [0, 1],
            prefix: "1-",
            as_socket: SUT,
            peer_socket: CALLEE,
            shared_hop: false,
            dt_us: 10_000
          }
        ]
      }
    ]
  )

/**
 * A call the platform REFUSED: one caller leg, answered 480 with its release
 * cause 253 ms in and never dialled onward, so the cut holds no called leg at
 * all.
 */
export const refusedFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 480, reason: "Temporarily Not Available", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 254, toTag: "sut-tag", headers: ["Reason: Q.850;cause=27"] }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 255, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/**
 * A call the CALLER abandoned: one caller leg, cancelled 19.7 s in and answered
 * the `200`+`487` pair RFC 3261 §9.2 owes it, with no called leg at any vantage.
 */
export const abandonedFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1 }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "CANCEL", src: CALLER, dst: SUT, ts_ms: 19_700 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: SUT, dst: CALLER, ts_ms: 19_702 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 19_703, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 19_720, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/**
 * The same race with the PLATFORM as the party that answers 200 after taking
 * the caller's CANCEL: one leg, so the only vantage is the caller's and the
 * violating 200 arrives as an `expect`.
 */
export const sutAnswersAfterCancelFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "CANCEL", src: CALLER, dst: SUT, ts_ms: 1_000 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: SUT, dst: CALLER, ts_ms: 1_010, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_050, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_055, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: CALLER, dst: SUT, ts_ms: 1_060, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 1_065, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/**
 * An answered call that then RENEGOTIATES: a re-INVITE inside the established
 * dialog, its own 200 and its own ACK. Two ACKs on one leg, both in-dialog, and
 * only the first confirms the dialog (§6.1).
 */
export const reInviteFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 5_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5_050, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: CALLER, dst: SUT, ts_ms: 5_055, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: CALLER, dst: SUT, ts_ms: 9_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 9_005, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/** The two dialogs each leg of {@link twoForksAnsweredFlows} is answered under. */
export const FORK_TAGS = {
  caller: { first: "sut-fork-a", second: "sut-fork-b" },
  callee: { first: "callee-fork-a", second: "callee-fork-b" }
} as const

/**
 * One INVITE answered 2xx under TWO To-tags, on both legs of a relayed call:
 * two forks ring, then each answers. RFC 3261 §13.2.2.4 makes every 2xx to the
 * INVITE a dialog of its own that the UAC ACKs, so each leg carries two
 * dialog-creating finals and two confirming ACKs; the caller BYEs the first
 * dialog as soon as it is confirmed (seq 2), keeps the second, re-INVITEs it
 * (seq 3), whose 200 and ACK sit inside a dialog already up, and BYEs it
 * (seq 4). The callee leg carries the same shape from the answering side.
 */
export const twoForksAnsweredFlows = (): Flows.FlowsDoc => {
  const a = FORK_TAGS.caller
  const b = FORK_TAGS.callee
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: a.first }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 400, toTag: a.second }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: a.first }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_010, toTag: a.first, branch: "z9hG4bK-ack-a-first" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_200, toTag: a.second }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_210, toTag: a.second, branch: "z9hG4bK-ack-a-second" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: CALLER, dst: SUT, ts_ms: 1_300, toTag: a.first }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 1_305, toTag: a.first }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 3_000, toTag: a.second }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 3_010, toTag: a.second }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "ACK", src: CALLER, dst: SUT, ts_ms: 3_015, toTag: a.second, branch: "z9hG4bK-ack-a-reinvite" }),
        request({ callId: CALLER_CALL_ID, seq: 4, method: "BYE", src: CALLER, dst: SUT, ts_ms: 9_000, toTag: a.second }),
        response({ callId: CALLER_CALL_ID, seq: 4, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 9_005, toTag: a.second })
      ]),
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 190, toTag: b.first }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 390, toTag: b.second }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 990, toTag: b.first }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_020, toTag: b.first, branch: "z9hG4bK-ack-b-first" }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_190, toTag: b.second }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_220, toTag: b.second, branch: "z9hG4bK-ack-b-second" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 1_310, toTag: b.first }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 1_315, toTag: b.first }),
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 3_005, toTag: b.second }),
        response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 3_008, toTag: b.second }),
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 3_020, toTag: b.second, branch: "z9hG4bK-ack-b-reinvite" }),
        request({ callId: CALLEE_CALL_ID, seq: 4, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 9_010, toTag: b.second }),
        response({ callId: CALLEE_CALL_ID, seq: 4, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 9_015, toTag: b.second })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * A re-INVITE sent OVER an un-ACKed 2xx, on both legs of a relayed call. The
 * caller re-INVITEs (seq 2) before ACKing the dialog-creating 200 (seq 1); the
 * platform answers 491 (RFC 3261 §14.1), the caller ACKs the 491 — that ACK is
 * the re-INVITE transaction's (§17.1.1.3) — and only then ACKs the 200, which
 * is the ACK that confirms the dialog (§13.2.2.4). The callee leg carries the
 * same shape from the answering side: the platform re-INVITEs before its own
 * ACK, the callee answers 491, and the 200's ACK is the last of the two.
 */
export const reInviteOverUnackedFinalFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 1_020, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 491, reason: "Request Pending", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_030, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_035, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_200, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: CALLER, dst: SUT, ts_ms: 9_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 9_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 990, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 1_025, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 491, reason: "Request Pending", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_028, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_040, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_210, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 9_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 9_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )

/**
 * The delayed offer, and the two ACK classes beside it. The re-INVITE at seq 2
 * carries NO offer, so the answer rides the ACK that confirms its 200 (RFC 3261
 * §13.2.1); the re-INVITE at seq 3 is refused 488, and its ACK belongs to that
 * INVITE transaction (§17.1.1.3) and carries nothing.
 */
export const delayedOfferFlows = (): Flows.FlowsDoc => {
  const answer = {
    contentType: "application/sdp",
    text: "v=0\r\no=- 2 1 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\nm=audio 40002 RTP/AVP 8\r\n"
  }
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_005, toTag: "sut-tag", headers: ["P-Charging-Vector: icid-value=abc"] }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 5_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5_050, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: CALLER, dst: SUT, ts_ms: 5_055, toTag: "sut-tag", body: answer }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 6_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 488, reason: "Not Acceptable Here", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 6_050, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "ACK", src: CALLER, dst: SUT, ts_ms: 6_055, toTag: "sut-tag", body: answer }),
        request({ callId: CALLER_CALL_ID, seq: 4, method: "BYE", src: CALLER, dst: SUT, ts_ms: 9_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 4, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 9_005, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )
}

/**
 * One retransmission ladder in its two spellings, which is what `repeat_of`
 * exists to unify: the platform repeats its 200 on ONE branch — `retx` sees that
 * — and the caller answers each copy with a fresh-branch ACK, which is a new
 * transaction every time and therefore never `retx`.
 */
export const reAckedFinalFlows = (): Flows.FlowsDoc => {
  const ok = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms, toTag: "sut-tag" })
  const ack = (ts_ms: number, branch: string): Flows.Msg =>
    request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms, toTag: "sut-tag", branch })
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag" }),
        ok(1_000),
        { ...ok(1_500), retx: true, repeat_of: 3 },
        { ...ok(2_500), retx: true, repeat_of: 3 },
        ack(2_600, "z9hG4bK-ack-1"),
        { ...ack(2_601, "z9hG4bK-ack-2"), repeat_of: 6 },
        { ...ack(2_602, "z9hG4bK-ack-3"), repeat_of: 6 },
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: CALLER, dst: SUT, ts_ms: 3_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 3_005, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )
}

/** The same capture from a producer that never computed `repeat_of`. */
export const withoutRepeatOf = (flows: Flows.FlowsDoc): Flows.FlowsDoc => ({
  ...flows,
  legs: flows.legs.map((l) => ({
    ...l,
    msgs: l.msgs.map(({ repeat_of: _dropped, ...rest }) => rest as Flows.Msg)
  }))
})

/**
 * A call the SUT only ever DIALLED: the b-leg is in the capture and the a-leg it
 * was minted from is not. There is no vantage the call arrived at, so the cut
 * refuses the whole family as a capture artifact.
 */
export const orphanBLegFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 190, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_000, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_005, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 5_000, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 5_005, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/**
 * TWO calls the capture keyed onto ONE Call-ID, as `capture_5e3fd853` shows
 * them: the first arrives on the SUT's :5060 node, is answered 404 and ACKed,
 * and the second arrives 40 ms later on its :5061 node — a fresh dialog with its
 * own outbound leg, not a second attempt of the first. Both ingress dialogs live
 * on leg 0 and both egress dialogs on leg 1, one hop each.
 */
export const twoIngressOneLegFlows = (): Flows.FlowsDoc => {
  const CALLER_B = "10.0.0.9:5061"
  const SUT_B = "10.0.0.1:5061"
  const CALLEE_B = "10.0.0.2:5061"
  return doc(
    [
      leg(CALLER_CALL_ID, [{ a: CALLER, b: SUT }, { a: CALLER_B, b: SUT_B }], [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_030, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_034, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER_B, dst: SUT_B, ts_ms: 1_040, hop: 1, branch: "z9hG4bK-second-ingress" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: SUT_B, dst: CALLER_B, ts_ms: 3_395, hop: 1, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER_B, dst: SUT_B, ts_ms: 3_397, hop: 1, toTag: "sut-tag", branch: "z9hG4bK-second-ingress" })
      ]),
      leg(CALLEE_CALL_ID, [{ a: SUT, b: CALLEE }, { a: SUT_B, b: CALLEE_B }], [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 62 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_010, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_012, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: SUT_B, dst: CALLEE_B, ts_ms: 1_099, hop: 1, fromTag: "second-egress" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: CALLEE_B, dst: SUT_B, ts_ms: 3_376, hop: 1, toTag: "callee-tag-b" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: SUT_B, dst: CALLEE_B, ts_ms: 3_378, hop: 1, toTag: "callee-tag-b", fromTag: "second-egress" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * Two ingress dialogs at two SUT instances, INTERLEAVED: the platform dials the
 * FIRST call's b-leg only after the SECOND call has already arrived. Pairing on
 * time gives that b-leg to the wrong ingress; pairing on the SUT socket cannot,
 * because an instance answers only what reached its own port.
 */
export const interleavedIngressFlows = (): Flows.FlowsDoc => {
  const CALLER_B = "10.0.0.9:5061"
  const SUT_B = "10.0.0.1:5061"
  const CALLEE_B = "10.0.0.2:5061"
  return doc(
    [
      leg(CALLER_CALL_ID, [{ a: CALLER, b: SUT }, { a: CALLER_B, b: SUT_B }], [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER_B, dst: SUT_B, ts_ms: 100, hop: 1, branch: "z9hG4bK-ingress-b" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 900, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 905, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: SUT_B, dst: CALLER_B, ts_ms: 950, hop: 1, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER_B, dst: SUT_B, ts_ms: 955, hop: 1, toTag: "sut-tag", branch: "z9hG4bK-ingress-b" })
      ]),
      leg(CALLEE_CALL_ID, [{ a: SUT, b: CALLEE }, { a: SUT_B, b: CALLEE_B }], [
        // The FIRST call's b-leg, dialled 100 ms after the second call arrived.
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 200 }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: SUT_B, dst: CALLEE_B, ts_ms: 300, hop: 1, fromTag: "egress-b" }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 880, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 885, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 404, reason: "Not Found", cseqMethod: "INVITE", src: CALLEE_B, dst: SUT_B, ts_ms: 930, hop: 1, toTag: "callee-tag-b" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: SUT_B, dst: CALLEE_B, ts_ms: 935, hop: 1, toTag: "callee-tag-b", fromTag: "egress-b" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * A capture that starts MID-DIALOG: the leg crosses the SUT's boundary and
 * carries an INVITE, but that INVITE names an established dialog, so no dialog
 * opens at the SUT and no case is proposed.
 */
export const midDialogOnlyFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 8, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0, toTag: "far-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 8, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 40, toTag: "far-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 8, method: "ACK", src: CALLER, dst: SUT, ts_ms: 45, toTag: "far-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 9, method: "BYE", src: CALLER, dst: SUT, ts_ms: 900, toTag: "far-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 9, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 905, toTag: "far-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/** A keepalive exchange at the SUT's boundary: no INVITE anywhere, so not a call. */
export const keepaliveOnlyFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg("keepalive-call-id", oneHop(CALLER, SUT), [
        request({ callId: "keepalive-call-id", seq: 1, method: "OPTIONS", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: "keepalive-call-id", seq: 1, status: 200, reason: "OK", cseqMethod: "OPTIONS", src: SUT, dst: CALLER, ts_ms: 5 })
      ])
    ],
    [{ legs: [0] }]
  )

/** A leg between two systems that are BOTH outside the SUT set: never ours. */
export const foreignLegFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg("foreign-call-id", oneHop(CALLEE, OTHER), [
        request({ callId: "foreign-call-id", seq: 1, method: "INVITE", src: CALLEE, dst: OTHER, ts_ms: 0 }),
        response({ callId: "foreign-call-id", seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: OTHER, dst: CALLEE, ts_ms: 100, toTag: "other-tag" }),
        request({ callId: "foreign-call-id", seq: 1, method: "ACK", src: CALLEE, dst: OTHER, ts_ms: 105, toTag: "other-tag" }),
        request({ callId: "foreign-call-id", seq: 2, method: "BYE", src: CALLEE, dst: OTHER, ts_ms: 900, toTag: "other-tag" }),
        response({ callId: "foreign-call-id", seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: OTHER, dst: CALLEE, ts_ms: 905, toTag: "other-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/** RFC 3262 §3's marker PAIR: what makes a provisional reliable on the wire. */
export const RELIABLE_RSEQ = 264_174_188

/** The answer a reliable provisional states, and the plan the 2xx must repeat. */
export const ANSWER_SDP =
  "v=0\r\no=sut 1 1 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\nm=audio 41000 RTP/AVP 8\r\n"

/** The same media at another address and port — a plan the first answer did not state. */
export const MOVED_SDP =
  "v=0\r\no=sut 1 1 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.11\r\nt=0 0\r\nm=audio 41004 RTP/AVP 8\r\n"

/**
 * The PLATFORM answering twice on one dialog: a reliable 183 states the answer
 * and the 200 states another plan (RFC 3261 §13.2.1). `reliable` is false where
 * the 183 carries no `100rel` pair — early media then, which binds nothing, so
 * the 200 is the dialog's FIRST answer and no second one exists.
 */
export const secondAnswerFlows = (reliable: boolean): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0, headers: ["Supported: 100rel"] }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 183, reason: "Session Progress", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag", sdp: ANSWER_SDP, headers: reliable ? [`Require: 100rel`, `RSeq: ${RELIABLE_RSEQ}`] : [] }),
        ...(reliable
          ? [request({ callId: CALLER_CALL_ID, seq: 2, method: "PRACK", src: CALLER, dst: SUT, ts_ms: 210, toTag: "sut-tag", headers: [`RAck: ${RELIABLE_RSEQ} 1 INVITE`] }),
            response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "PRACK", src: SUT, dst: CALLER, ts_ms: 215, toTag: "sut-tag" })]
          : []),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: "sut-tag", sdp: MOVED_SDP }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: CALLER, dst: SUT, ts_ms: 6_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 6_005, toTag: "sut-tag" })
      ])
    ],
    [{ legs: [0] }]
  )

/**
 * A call whose reliable 183 nobody PRACKs, across a B2BUA. Leg 0 is the caller's
 * dialog, where the CALLER took the provisional; leg 1 the callee's, where the
 * PLATFORM took it. Which vantage a case is cut from decides which of the two
 * the document charges.
 */
export const unackedProvisionalFlows = (): Flows.FlowsDoc => {
  const reliable = [`Require: 100rel`, `RSeq: ${RELIABLE_RSEQ}`]
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0, headers: ["Supported: 100rel"] }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 183, reason: "Session Progress", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 200, toTag: "sut-tag", headers: reliable }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 2_200, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 2_205, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: CALLER, dst: SUT, ts_ms: 6_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 6_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 10, headers: ["Supported: 100rel"] }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 183, reason: "Session Progress", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 190, toTag: "callee-tag", headers: reliable }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 2_190, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 2_195, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 6_002, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 6_007, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * A sequential hunt as the CORRELATION engine sees it: two call groups, the
 * first answered 486, the second dialled 900 ms later to a different callee.
 * Each group states its own initial INVITE and terminal status, because those
 * are the extractor's verdicts and not the engine's to re-derive.
 */
export const rerouteFlows = (): Flows.FlowsDoc => {
  const FIRST = "attempt-1-call-id"
  const SECOND = "attempt-2-call-id"
  const firstCallee = "sip:+33600000004@10.0.0.1"
  const secondCallee = "sip:+33600000005@10.0.0.1"
  return doc(
    [
      leg(FIRST, oneHop(SUT, CALLEE), [
        request({ callId: FIRST, seq: 1, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 0, ruri: firstCallee }),
        response({ callId: FIRST, seq: 1, status: 486, reason: "Busy Here", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 400, toTag: "callee-tag" }),
        request({ callId: FIRST, seq: 1, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 405, toTag: "callee-tag", ruri: firstCallee })
      ]),
      leg(SECOND, oneHop(SUT, OTHER), [
        request({ callId: SECOND, seq: 1, method: "INVITE", src: SUT, dst: OTHER, ts_ms: 1_300, ruri: secondCallee }),
        response({ callId: SECOND, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: OTHER, dst: SUT, ts_ms: 2_000, toTag: "other-tag" }),
        request({ callId: SECOND, seq: 1, method: "ACK", src: SUT, dst: OTHER, ts_ms: 2_005, toTag: "other-tag", ruri: secondCallee }),
        request({ callId: SECOND, seq: 2, method: "BYE", src: SUT, dst: OTHER, ts_ms: 9_000, toTag: "other-tag", ruri: secondCallee }),
        response({ callId: SECOND, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: OTHER, dst: SUT, ts_ms: 9_005, toTag: "other-tag" })
      ])
    ],
    [
      { legs: [0], t0_us: 0, initial_invite: { leg: 0, msg: 0 }, final_status: 486, final_us: 400_000 },
      { legs: [1], t0_us: 1_300_000, initial_invite: { leg: 1, msg: 0 }, final_status: 200, final_us: 2_000_000 }
    ]
  )
}

/**
 * The SUT HAIRPINS a call through the network, as
 * `capture_019089e6-b01a-432d-9d5c-41fecd2979a3` shows it: the b-leg it dials
 * out (leg 1) is routed straight back to it, so the SAME Call-ID opens a dialog
 * in each direction at the one boundary hop, and the SUT then dials leg 2 for
 * the transit it made of its own INVITE.
 */
export const hairpinLoopbackFlows = (): Flows.FlowsDoc => {
  const THIRD = `1-${CALLEE_CALL_ID}`
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(CALLER, SUT), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 2_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 2_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: SUT, dst: CALLER, ts_ms: 9_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLER, dst: SUT, ts_ms: 9_005, toTag: "sut-tag" })
      ]),
      // Dialled out at 60 ms and back at the SUT 120 ms later, same Call-ID and
      // From tag: the network routed the SUT's own INVITE home.
      leg(CALLEE_CALL_ID, oneHop(SUT, CALLEE), [
        request({ callId: CALLEE_CALL_ID, seq: 5, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 60 }),
        request({ callId: CALLEE_CALL_ID, seq: 5, method: "INVITE", src: CALLEE, dst: SUT, ts_ms: 180, branch: "z9hG4bK-looped-home" }),
        response({ callId: CALLEE_CALL_ID, seq: 5, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLEE, ts_ms: 1_900, toTag: "hairpin-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 5, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_905, toTag: "hairpin-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 5, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_950, toTag: "hairpin-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 5, method: "ACK", src: CALLEE, dst: SUT, ts_ms: 1_955, toTag: "hairpin-tag" })
      ]),
      leg(THIRD, oneHop(SUT, CALLEE), [
        request({ callId: THIRD, seq: 9, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 240 }),
        response({ callId: THIRD, seq: 9, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 1_800, toTag: "third-tag" }),
        request({ callId: THIRD, seq: 9, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 1_805, toTag: "third-tag" }),
        request({ callId: THIRD, seq: 10, method: "BYE", src: CALLEE, dst: SUT, ts_ms: 8_900, toTag: "third-tag" }),
        response({ callId: THIRD, seq: 10, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLEE, ts_ms: 8_905, toTag: "third-tag" })
      ])
    ],
    [{ legs: [0, 1, 2] }]
  )
}

/**
 * A plain call and a hairpin in ONE capture, as
 * `capture_019089e6-b01a-432d-9d5c-41fecd2979a3` holds them: two families, the
 * first cut as a case and the second refused. The selection file names both, and
 * a name is claimed once — so the refusal must not read the first family's.
 */
export const hairpinBesideAPlainCallFlows = (): Flows.FlowsDoc => {
  const PLAIN = "plain-call-call-id"
  const PLAIN_B = `1-${PLAIN}`
  const hairpin = hairpinLoopbackFlows()
  return doc(
    [
      leg(PLAIN, oneHop(CALLER, SUT), [
        request({ callId: PLAIN, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: PLAIN, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: PLAIN, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: PLAIN, seq: 2, method: "BYE", src: CALLER, dst: SUT, ts_ms: 5_000, toTag: "sut-tag" }),
        response({ callId: PLAIN, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT, dst: CALLER, ts_ms: 5_005, toTag: "sut-tag" })
      ]),
      leg(PLAIN_B, oneHop(SUT, CALLEE), [
        request({ callId: PLAIN_B, seq: 7, method: "INVITE", src: SUT, dst: CALLEE, ts_ms: 20 }),
        response({ callId: PLAIN_B, seq: 7, status: 200, reason: "OK", cseqMethod: "INVITE", src: CALLEE, dst: SUT, ts_ms: 990, toTag: "callee-tag" }),
        request({ callId: PLAIN_B, seq: 7, method: "ACK", src: SUT, dst: CALLEE, ts_ms: 995, toTag: "callee-tag" }),
        request({ callId: PLAIN_B, seq: 8, method: "BYE", src: SUT, dst: CALLEE, ts_ms: 5_010, toTag: "callee-tag" }),
        response({ callId: PLAIN_B, seq: 8, status: 200, reason: "OK", cseqMethod: "BYE", src: CALLEE, dst: SUT, ts_ms: 5_015, toTag: "callee-tag" })
      ]),
      ...hairpin.legs
    ],
    [{ legs: [0, 1] }, { legs: [2, 3, 4] }]
  )
}

/**
 * Two ingress dialogs at two SUT instances on ONE captured leg, where the
 * second one's in-dialog TAIL crosses at a third boundary hop — the peer answers
 * from a second address of its own. Both anchors ran the same way at the SUT, so
 * direction cannot say which of them the tail continues; only its socket can.
 */
export const secondIngressTailFlows = (): Flows.FlowsDoc => {
  const SUT_B = "10.0.0.1:5061"
  return doc(
    [
      leg(CALLER_CALL_ID, [{ a: CALLER, b: SUT }, { a: CALLER, b: SUT_B }, { a: OTHER, b: SUT_B }], [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 501, reason: "Not Implemented", cseqMethod: "INVITE", src: SUT, dst: CALLER, ts_ms: 50, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: CALLER, dst: SUT, ts_ms: 55, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: CALLER, dst: SUT_B, ts_ms: 100, hop: 1, branch: "z9hG4bK-ingress-b" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: SUT_B, dst: CALLER, ts_ms: 900, hop: 1, toTag: "sut-tag-b" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: OTHER, dst: SUT_B, ts_ms: 905, hop: 2, toTag: "sut-tag-b", branch: "z9hG4bK-ingress-b" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: OTHER, dst: SUT_B, ts_ms: 5_000, hop: 2, toTag: "sut-tag-b" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: SUT_B, dst: OTHER, ts_ms: 5_005, hop: 2, toTag: "sut-tag-b" })
      ])
    ],
    [{ legs: [0] }]
  )
}
