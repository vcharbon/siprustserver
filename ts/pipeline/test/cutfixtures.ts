/**
 * Synthetic schema-5 flows documents, built datagram by datagram, for the
 * refusal rules that read a call family and its termination.
 *
 * `SUT_IP` is the system under test, reached at `SUT_INGRESS` and dialling
 * out from `SUT_EGRESS`; `UPSTREAM` is the caller's peer and `FAR` the far
 * end. Every address is a documentation range.
 */
import type { Flows } from "@sip/contracts"

const CRLF = "\r\n"

export const SUT_IP = "192.0.2.10"
export const UPSTREAM = `198.51.100.10:5060`
export const FAR = "203.0.113.89:5060"
export const SUT_INGRESS = `${SUT_IP}:5072`
export const SUT_EGRESS = `${SUT_IP}:5062`

export const BASE_CALL_ID = "base-call-0001@198.51.100.4"
export const DERIVED_CALL_ID = `1-${BASE_CALL_ID}`

interface Wire {
  readonly callId: string
  /** Which probe wrote this copy; default `0`, the one observation point. */
  readonly probe?: number
  readonly seq: number
  readonly method: string
  readonly src: string
  readonly dst: string
  readonly hop: number
  readonly ts_ms: number
  readonly status?: number
  /** Absent means the request opens a dialog, which is what anchors a vantage. */
  readonly toTag?: string
}

const URI = "sip:+15550100001@example.invalid"

const identityOf = (uri: string): Flows.Identity => {
  const at = uri.indexOf("@")
  const colon = uri.indexOf(":")
  const user = at < 0 ? null : uri.slice(colon + 1, at)
  return { uri, user, digits: user === null ? null : user.replace(/[^0-9]/g, "") }
}

export const msg = (w: Wire): Flows.Msg => {
  const start =
    w.status === undefined ? `${w.method} ${URI} SIP/2.0` : `SIP/2.0 ${w.status} Testing`
  const raw = [
    start,
    `Via: SIP/2.0/UDP ${w.status === undefined ? w.src : w.dst};branch=z9hG4bK-${w.callId}-${w.seq}-${w.method}`,
    `From: <sip:caller@example.invalid>;tag=from-${w.callId}`,
    `To: <${URI}>${w.toTag === undefined ? "" : `;tag=${w.toTag}`}`,
    `Call-ID: ${w.callId}`,
    `CSeq: ${w.seq} ${w.method}`,
    "Content-Length: 0",
    "",
    ""
  ].join(CRLF)
  return {
    ts_us: w.ts_ms * 1000,
    src: w.src,
    dst: w.dst,
    hop: w.hop,
    retx: false,
    probe: w.probe ?? 0,
    raw,
    identities: {
      from: identityOf("sip:caller@example.invalid"),
      to: identityOf(URI),
      ruri: identityOf(URI)
    },
    summary:
      w.status === undefined
        ? {
            kind: "request",
            method: w.method,
            uri: URI,
            cseq: { seq: w.seq, method: w.method },
            from: { uri: "sip:caller@example.invalid", tag: `from-${w.callId}` },
            to: { uri: URI, tag: w.toTag ?? null }
          }
        : {
            kind: "response",
            status: w.status,
            reason: "Testing",
            cseq: { seq: w.seq, method: w.method },
            from: { uri: "sip:caller@example.invalid", tag: `from-${w.callId}` },
            to: { uri: URI, tag: w.toTag ?? null }
          }
  }
}

/**
 * `final_status` and `terminated_by` are FIXED at `200` / `null` whatever the
 * datagrams say. Deliberate: every consumer that reads a leg's outcome off its
 * own messages proves here that it does, and one that reached for the summary
 * would answer the same thing about every fixture in this file.
 */
export const leg = (
  callId: string,
  hops: ReadonlyArray<Flows.Hop>,
  msgs: ReadonlyArray<Flows.Msg>
): Flows.Leg => ({
  call_id: callId,
  hops,
  invite: { ruri: URI, from_uri: "sip:caller@example.invalid", to_uri: URI, cseq: 1 },
  final_status: 200,
  saw_180: true,
  terminated_by: null,
  tokens: [],
  msgs
})

/** How the capture LEFT one dialog, which is what completeness reads. */
export type DialogEnd =
  /** Answered and closed: the only complete outcome for an established dialog. */
  | "torn-down"
  /** Answered and left up when the capture stopped. */
  | "left-open"
  /** Rang, then answered and closed: the callee's 180 precedes its 200. */
  | "rung-then-torn-down"
  /** An error final: the attempt failed, and a failed attempt is finished. */
  | "rejected"
  /** CANCEL, then the 487 it asked for. */
  | "cancelled"
  /** RFC 3261 §9.1's race: the CANCEL lost, the 2xx is out, and no BYE follows. */
  | "cancelled-but-answered"
  /** Closed, then the 2xx repeated: a retransmission re-opens nothing. */
  | "retransmitted-after-bye"
  /** Closed, then a SECOND attempt answered and left up. */
  | "re-answered-after-teardown"
  /** The capture stopped inside the INVITE transaction: a 100, and no final. */
  | "unanswered"
  /** The return direction is missing: the INVITE, and nothing at all coming back. */
  | "silent"

/**
 * One dialog at one hop, as the capture holds it. `from` sends the INVITE, so
 * it is the side that opened the dialog and `to` is the side that answers.
 */
export const dialogMsgs = (o: {
  readonly callId: string
  readonly hop: number
  readonly from: string
  readonly to: string
  readonly at_ms: number
  readonly toTag: string
  readonly end: DialogEnd
}): ReadonlyArray<Flows.Msg> => {
  const { at_ms, callId, end, from, hop, to, toTag } = o
  const req = (method: string, seq: number, ms: number, tag?: string): Flows.Msg =>
    msg({ callId, seq, method, src: from, dst: to, hop, ts_ms: at_ms + ms, toTag: tag })
  const res = (status: number, method: string, seq: number, ms: number): Flows.Msg =>
    msg({ callId, seq, method, status, src: to, dst: from, hop, ts_ms: at_ms + ms, toTag })
  const opening = req("INVITE", 1, 0)
  const answered = [opening, res(200, "INVITE", 1, 900), req("ACK", 1, 905, toTag)]
  const bye = [req("BYE", 2, 5_000, toTag), res(200, "BYE", 2, 5_005)]
  switch (end) {
    case "torn-down":
      return [...answered, ...bye]
    case "left-open":
      return answered
    case "rung-then-torn-down":
      return [opening, res(180, "INVITE", 1, 200), ...answered.slice(1), ...bye]
    case "rejected":
      return [opening, res(486, "INVITE", 1, 300), req("ACK", 1, 305, toTag)]
    case "cancelled":
      return [
        opening,
        res(180, "INVITE", 1, 200),
        req("CANCEL", 1, 400),
        res(200, "CANCEL", 1, 405),
        res(487, "INVITE", 1, 410),
        req("ACK", 1, 415, toTag)
      ]
    case "cancelled-but-answered":
      return [
        opening,
        res(180, "INVITE", 1, 200),
        req("CANCEL", 1, 400),
        res(200, "CANCEL", 1, 405),
        res(200, "INVITE", 1, 410),
        req("ACK", 1, 415, toTag)
      ]
    case "retransmitted-after-bye":
      return [...answered, ...bye, { ...res(200, "INVITE", 1, 5_100), retx: true }]
    case "re-answered-after-teardown":
      return [
        ...answered,
        ...bye,
        req("INVITE", 3, 6_000),
        res(200, "INVITE", 3, 6_900),
        req("ACK", 3, 6_905, toTag)
      ]
    case "unanswered":
      // A 100 carries no To-tag: it opens no dialog and ends no transaction.
      return [
        opening,
        msg({ callId, seq: 1, method: "INVITE", status: 100, src: to, dst: from, hop, ts_ms: at_ms + 5 })
      ]
    case "silent":
      // Not even the 100 an unanswered attempt draws: this hop's return
      // direction is the one the capture missed.
      return [opening]
  }
}

/** The envelope `sipflow --json` puts around a document; zeroed, since no test reads it. */
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

/** The derivation evidence the extractor emits for a `1-<base>` pair. */
const derivedEvidence = (legs: ReadonlyArray<number>): Flows.Evidence => ({
  kind: "derived_call_id",
  strategy: 0,
  legs,
  prefix: "1-",
  as_socket: SUT_INGRESS,
  peer_socket: UPSTREAM,
  shared_hop: false,
  dt_us: 10_000
})

/**
 * The completeness gate's geometry: the peer opens a dialog INTO the SUT (leg 0)
 * and the SUT dials out on a derived Call-ID (leg 1). `derivedArrives` turns that
 * second INVITE around, so BOTH vantages are caller-side ingress and the case is
 * the multi-call shape.
 */
export const completenessFlows = (o: {
  readonly caller: DialogEnd
  readonly derived: DialogEnd
  readonly derivedArrives?: boolean
  /** When the derived leg opens, in ms after the caller's INVITE; 10 unless said. */
  readonly derivedAtMs?: number
}): Flows.FlowsDoc =>
  doc(
    [
      leg(
        BASE_CALL_ID,
        [{ a: UPSTREAM, b: SUT_INGRESS }],
        dialogMsgs({
          callId: BASE_CALL_ID,
          hop: 0,
          from: UPSTREAM,
          to: SUT_INGRESS,
          at_ms: 0,
          toTag: "sut-tag",
          end: o.caller
        })
      ),
      leg(
        DERIVED_CALL_ID,
        [o.derivedArrives === true ? { a: UPSTREAM, b: SUT_INGRESS } : { a: SUT_EGRESS, b: FAR }],
        dialogMsgs({
          callId: DERIVED_CALL_ID,
          hop: 0,
          from: o.derivedArrives === true ? UPSTREAM : SUT_EGRESS,
          to: o.derivedArrives === true ? SUT_INGRESS : FAR,
          at_ms: o.derivedAtMs ?? 10,
          toTag: o.derivedArrives === true ? "sut-tag-2" : "far-tag",
          end: o.derived
        })
      )
    ],
    [{ legs: [0, 1], evidence: [derivedEvidence([0, 1])] }]
  )
