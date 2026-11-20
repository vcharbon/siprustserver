/**
 * The answered call the recorded samples play (`README.md`, "The recorded
 * samples"): Alice Example calls Bob Example by number through the system,
 * Bob rings and answers, and Alice hangs up (RFC 3261 §13, §15). A variant
 * adds headers to the caller's INVITE, or has the caller put the call on
 * hold with a re-INVITE before hanging up (RFC 3264 §8.4): a sendonly offer
 * answered recvonly, each session description's version one past its
 * party's last (§8).
 *
 * Each party states its display name on its From, To and Contact, and its
 * number in its URIs' user parts, so the capture carries identities a
 * rewrite can reach. Leg `A` is the caller, leg `B` the callee side as the
 * system sees it.
 */
import { ANSWER, callDocumentOf, CallFlow, idOf, OFFER, RESOURCES, REWRITE } from "@sip/basic-case"
import type { CompiledCell } from "@sip/rig"

/** A message as the step builder states it. */
type MsgSpec = Parameters<CallFlow["send"]>[1]

/** A header as a message states it. */
type Header = NonNullable<MsgSpec["headers"]>[number]

/** The caller: its display name, number and URI. */
export const CALLER = { display: "Alice Example", number: "+15555550100", uri: "sip:+15555550100@atlanta.example.com" } as const

/** The callee: its display name, number and URI. */
export const CALLEE = { display: "Bob Example", number: "+15555550101", uri: "sip:+15555550101@biloxi.example.com" } as const

/** The methods each party accepts (RFC 3261 §20.5) and the extensions it supports, none (§20.37). */
const CAPABILITIES: ReadonlyArray<Header> = [
  { name: "Allow", value: "INVITE, ACK, CANCEL, BYE, OPTIONS" },
  { name: "Supported", value: "" }
]

/** How long the callee takes to ring once the INVITE reaches it, in ms. */
const RING_MS = 20

/** How long the callee rings before it answers, in ms. */
const ANSWER_MS = 40

/** How long the confirmed call lasts before each of the caller's in-dialog requests, in ms. */
const HOLD_MS = 100

/** The resource path of the caller's hold offer. */
const HOLD_OFFER = "resources/hold-offer.sdp"

/** The resource path of the callee's answer to the hold offer. */
const HOLD_ANSWER = "resources/hold-answer.sdp"

/**
 * A session description from `address`, its media on `port` in `direction`,
 * at `version` of the party's session (RFC 4566 §5.2, RFC 3264 §8).
 */
const sdp = (address: string, port: number, version: number, direction: string): string =>
  [
    "v=0",
    `o=- 1 ${version} IN IP4 ${address}`,
    "s=-",
    `c=IN IP4 ${address}`,
    "t=0 0",
    `m=audio ${port} RTP/AVP 0`,
    "a=rtpmap:0 PCMU/8000",
    `a=${direction}`,
    ""
  ].join("\r\n")

/** The resources of the answered call: the form's offer and answer, and the hold offer and answer at version 2. */
const CALL_RESOURCES: Readonly<Record<string, string>> = {
  ...RESOURCES,
  [HOLD_OFFER]: sdp("192.0.2.10", 49170, 2, "sendonly"),
  [HOLD_ANSWER]: sdp("192.0.2.20", 49172, 2, "recvonly")
}

/** The name-addr a party states on its From or To. */
const nameAddr = (party: typeof CALLER | typeof CALLEE): string => `"${party.display}" <${party.uri}>`

/** What varies between the recorded answered calls. */
export interface Variant {
  /** The case's title, and the cell's name. */
  readonly title: string
  /** What the call plays beside the answered call, for the document's note. */
  readonly note: string
  /** Headers the caller's INVITE carries beside its capabilities. */
  readonly inviteHeaders?: ReadonlyArray<Header>
  /** The caller holds the call with a re-INVITE before hanging up. */
  readonly holds?: boolean
}

/**
 * The caller's re-INVITE holding the confirmed call from `after`, carried to
 * the callee, answered and acknowledged on both legs (RFC 3261 §14, CSeq 2):
 * the id of the callee's step receiving the ACK.
 */
const hold = (flow: CallFlow, after: string): string => {
  const reinvite = flow.send("A", {
    method: "INVITE",
    headers: [...CAPABILITIES],
    body: { ref: HOLD_OFFER, rewrite: [...REWRITE] }
  }, { from: after, ms: HOLD_MS })
  flow.expect("A", { status: 100, reason: "Trying", "cseq-method": "INVITE", cseq: 2 }, { from: reinvite, auto: true })
  const reached = flow.expect("B", { method: "INVITE", body: { mode: "sdp-present" } }, { from: reinvite })
  const ok = flow.send("B", {
    status: 200,
    reason: "OK",
    "cseq-method": "INVITE",
    headers: [...CAPABILITIES],
    body: { ref: HOLD_ANSWER, rewrite: [...REWRITE] }
  }, { from: reached })
  const answered = flow.expect("A", { status: 200, reason: "OK", "cseq-method": "INVITE", body: { mode: "sdp-present" } }, { from: ok })
  const ack = flow.send("A", { method: "ACK", cseq: 2 }, { from: answered, auto: true })
  return flow.expect("B", { method: "ACK", cseq: 2 }, { from: ack, auto: true })
}

/** The steps of the answered call `variant` plays. */
const flowOf = (variant: Variant): CallFlow => {
  const flow = new CallFlow()
  const invite = flow.send("A", {
    method: "INVITE",
    ruri: { frozen: CALLEE.uri },
    from: { frozen: CALLER.uri, addr: nameAddr(CALLER) },
    to: { frozen: CALLEE.uri, addr: nameAddr(CALLEE) },
    headers: [...CAPABILITIES, ...(variant.inviteHeaders ?? [])],
    body: { ref: OFFER, rewrite: [...REWRITE] },
    contact: { display: CALLER.display, user: CALLER.number }
  })
  flow.expect("A", { status: 100, reason: "Trying", "cseq-method": "INVITE", cseq: 1 }, { from: invite, auto: true })
  const reached = flow.expect("B", { method: "INVITE", body: { mode: "sdp-present" } }, { from: invite })
  const ringing = flow.send("B", {
    status: 180,
    reason: "Ringing",
    "cseq-method": "INVITE",
    contact: { display: CALLEE.display, user: CALLEE.number }
  }, { from: reached, ms: RING_MS })
  flow.expect("A", { status: 180, reason: "Ringing", "cseq-method": "INVITE" }, { from: ringing })
  const ok = flow.send("B", {
    status: 200,
    reason: "OK",
    "cseq-method": "INVITE",
    headers: [...CAPABILITIES],
    body: { ref: ANSWER, rewrite: [...REWRITE] }
  }, { from: ringing, ms: ANSWER_MS })
  const answered = flow.expect("A", { status: 200, reason: "OK", "cseq-method": "INVITE", body: { mode: "sdp-present" } }, {
    from: ok
  })
  const ack = flow.send("A", { method: "ACK", cseq: 1 }, { from: answered, auto: true })
  const acked = flow.expect("B", { method: "ACK", cseq: 1 }, { from: ack, auto: true })
  const up = variant.holds === true ? hold(flow, acked) : acked
  const bye = flow.send("A", { method: "BYE" }, { from: up, ms: HOLD_MS })
  const byeIn = flow.expect("B", { method: "BYE" }, { from: bye })
  const byeOk = flow.send("B", { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeIn })
  flow.expect("A", { status: 200, reason: "OK", "cseq-method": "BYE" }, { from: byeOk })
  return flow
}

/** The cell of `variant`, claiming the one call record the system writes of the call: bridged, ended by the caller's BYE. */
export const cellOf = (variant: Variant): CompiledCell.CompiledCell => ({
  id: idOf(variant.title),
  case: variant.title,
  title: variant.title,
  document: callDocumentOf({ title: variant.title, caller: CALLER.uri, callee: CALLEE.uri, note: variant.note }, flowOf(variant).steps),
  resources: CALL_RESOURCES,
  claims: {
    "call-records": {
      count: 1,
      checks: [
        { field: "b_legs.0.disposition", op: "eq", value: "bridged" },
        { field: "termination.cause", op: "eq", value: "remote_bye" }
      ]
    }
  }
})
