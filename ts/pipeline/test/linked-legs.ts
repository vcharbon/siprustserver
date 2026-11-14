/**
 * Calls through a B2BUA that gives the leg it dials a Call-ID of its own and
 * names the caller leg's Call-ID in a header of that leg's INVITE: legs no
 * Call-ID derivation relates, which a `header-pair` rule joins.
 *
 * Each document projects `Call-ID` and the linking header as `sipflow
 * --emit-headers` does: every message carries its Call-ID instance, and a
 * callee leg's INVITE its link, in the bytes and in the projection alike.
 */
import type { Flows, Rules } from "@sip/contracts"
import { doc, leg, oneHop, request, response, SOCKETS } from "./fixtures.js"

/** The header a callee leg's INVITE names its caller leg in. */
export const LINK_HEADER = "X-Linked-Call"

/** The rule joining a callee leg to the caller leg its {@link LINK_HEADER} names. */
export const LINK_RULE: Rules.HeaderPairRule = {
  kind: "header-pair",
  name: "linked-call",
  left: { header: "Call-ID", pattern: "^(?<key>\\S+)$" },
  right: { header: LINK_HEADER, pattern: "^(?<key>\\S+)$" }
}

/** The headers a document of linked legs projects. */
export const LINKED_HEADERS: ReadonlyArray<string> = ["Call-ID", LINK_HEADER]

/** One answered dialog, its INVITE at `t` ms carrying `extra` header lines. */
const answered = (
  callId: string,
  src: string,
  dst: string,
  t: number,
  extra: ReadonlyArray<string> = []
): ReadonlyArray<Flows.Msg> => [
  request({ callId, seq: 1, method: "INVITE", src, dst, ts_ms: t, headers: extra }),
  response({ callId, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: dst, dst: src, ts_ms: t + 100, toTag: `${callId}-tag` }),
  response({ callId, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: dst, dst: src, ts_ms: t + 1_000, toTag: `${callId}-tag` }),
  request({ callId, seq: 1, method: "ACK", src, dst, ts_ms: t + 1_005, toTag: `${callId}-tag` }),
  request({ callId, seq: 2, method: "BYE", src, dst, ts_ms: t + 5_000, toTag: `${callId}-tag` }),
  response({ callId, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: dst, dst: src, ts_ms: t + 5_005, toTag: `${callId}-tag` })
]

/** The caller's leg `callId`, into the SUT at `t` ms. */
export const callerLeg = (callId: string, t: number): Flows.Leg =>
  leg(callId, oneHop(SOCKETS.caller, SOCKETS.sut), answered(callId, SOCKETS.caller, SOCKETS.sut, t))

/**
 * The leg `callId` the SUT dials at `t` ms, its INVITE naming `link` in
 * `header`: in its bytes, and as the header instance a projection of `header`
 * keeps.
 */
export const calleeLeg = (callId: string, t: number, link: string, header: string = LINK_HEADER): Flows.Leg => {
  const [invite, ...rest] = answered(callId, SOCKETS.sut, SOCKETS.callee, t, [`${header}: ${link}`])
  return leg(callId, oneHop(SOCKETS.sut, SOCKETS.callee), [{ ...invite!, headers: [{ name: header, value: link }] }, ...rest])
}

/**
 * A document of `legs`, each its own call group, projecting `projected`: every
 * message carries its `Call-ID` where it is projected, and of the header
 * instances a leg states, those projected.
 */
export const linkedDoc = (
  legs: ReadonlyArray<Flows.Leg>,
  projected: ReadonlyArray<string> = LINKED_HEADERS
): Flows.FlowsDoc => {
  const named = new Set(projected.map((h) => h.toLowerCase()))
  const projection = (callId: string, msg: Flows.Msg): ReadonlyArray<Flows.HeaderInstance> => [
    ...(named.has("call-id") ? [{ name: "Call-ID", value: callId }] : []),
    ...(msg.headers ?? []).filter((h) => named.has(h.name.toLowerCase()))
  ]
  return {
    ...doc(
      legs.map((l) => ({ ...l, msgs: l.msgs.map((m) => ({ ...m, headers: projection(l.call_id, m) })) })),
      legs.map((_, i) => ({ legs: [i], initial_invite: { leg: i, msg: 0 } }))
    ),
    emit_headers: [...projected]
  }
}

/** `n` answered calls, 20 s apart, each a caller leg and a callee leg naming it in {@link LINK_HEADER}. */
export const linkedCallsFlows = (n: number): Flows.FlowsDoc =>
  linkedDoc(
    Array.from({ length: n }, (_, i) => {
      const caller = `caller-${i}@192.0.2.9`
      return [callerLeg(caller, i * 20_000), calleeLeg(`callee-${i}@192.0.2.1`, i * 20_000 + 10, caller)]
    }).flat()
  )

/** `rule`, counting how often a side's pattern is read: once per side each time the rule is compiled for a correlation. */
export const countingPatterns = (rule: Rules.HeaderPairRule) => {
  let n = 0
  const side = (s: Rules.HeaderSide): Rules.HeaderSide => ({
    header: s.header,
    get pattern() {
      n += 1
      return s.pattern
    }
  })
  return { rule: { ...rule, left: side(rule.left), right: side(rule.right) }, count: () => n }
}
