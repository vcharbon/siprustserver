/**
 * Calls through a B2BUA that gives the leg it dials a Call-ID of its own and
 * names the caller leg in a header of that leg's INVITE: legs no Call-ID
 * derivation relates, which a `header-pair` rule joins.
 *
 * Each document projects the headers it is told to as `sipflow
 * --emit-headers` does: of the header lines a leg's INVITE carries in its
 * bytes, those projected. `Call-ID` is never projected: a rule reads it off the
 * leg.
 */
import { Flows, type Rules } from "@sip/contracts"
import type { LegRules } from "../src/header-pair-join.js"
import { doc, leg, oneHop, request, response, SOCKETS } from "./fixtures.js"

/** The header a callee leg's INVITE names its caller leg's Call-ID in. */
export const LINK_HEADER = "X-Linked-Call"

/** The rule joining a callee leg to the caller leg whose Call-ID its {@link LINK_HEADER} names. */
export const LINK_RULE: Rules.HeaderPairRule = {
  kind: "header-pair",
  name: "linked-call",
  left: { header: "Call-ID", pattern: "^(?<key>\\S+)$" },
  right: { header: LINK_HEADER, pattern: "^(?<key>\\S+)$" }
}

/** The header a caller leg's INVITE carries a short reference in. */
export const REF_HEADER = "X-Ref"

/** The header a callee leg's INVITE names its caller leg's {@link REF_HEADER} in. */
export const PARENT_REF_HEADER = "X-Parent-Ref"

/** The rule joining a callee leg to the caller leg whose {@link REF_HEADER} its {@link PARENT_REF_HEADER} names. */
export const REF_RULE: Rules.HeaderPairRule = {
  kind: "header-pair",
  name: "parent-ref",
  left: { header: REF_HEADER, pattern: "^(?<key>\\d+)$" },
  right: { header: PARENT_REF_HEADER, pattern: "^(?<key>\\d+)$" }
}

/** The headers a document of linked legs projects. */
export const LINKED_HEADERS: ReadonlyArray<string> = [LINK_HEADER, REF_HEADER, PARENT_REF_HEADER]

/** `rules`, every one required of a document. */
export const requiring = (...rules: ReadonlyArray<Rules.HeaderPairRule>): LegRules => ({ rules, unprojected: "require" })

/** `rules`, every one skipped on a document not projecting its headers. */
export const skipping = (...rules: ReadonlyArray<Rules.HeaderPairRule>): LegRules => ({ rules, unprojected: "skip" })

/** One header line of an INVITE: its name and value. */
export type Line = readonly [name: string, value: string]

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

/** The leg `callId` from `src` to `dst`, its INVITE at `t` ms carrying `lines` in its bytes and as header instances. */
const legWith = (callId: string, src: string, dst: string, t: number, lines: ReadonlyArray<Line>): Flows.Leg => {
  const [invite, ...rest] = answered(callId, src, dst, t, lines.map(([name, value]) => `${name}: ${value}`))
  return leg(callId, oneHop(src, dst), [{ ...invite!, headers: lines.map(([name, value]) => ({ name, value })) }, ...rest])
}

/** The caller's leg `callId`, into the SUT at `t` ms, its INVITE carrying `lines`. */
export const callerLeg = (callId: string, t: number, lines: ReadonlyArray<Line> = []): Flows.Leg =>
  legWith(callId, SOCKETS.caller, SOCKETS.sut, t, lines)

/** The leg `callId` the SUT dials at `t` ms, its INVITE carrying `lines`. */
export const dialledLeg = (callId: string, t: number, lines: ReadonlyArray<Line>): Flows.Leg =>
  legWith(callId, SOCKETS.sut, SOCKETS.callee, t, lines)

/** The leg `callId` the SUT dials at `t` ms, its INVITE naming `link` in `header`. */
export const calleeLeg = (callId: string, t: number, link: string, header: string = LINK_HEADER): Flows.Leg =>
  dialledLeg(callId, t, [[header, link]])

/**
 * A document of `legs`, each its own call group, projecting `projected`: of
 * the header instances a leg's messages state, those projected.
 */
export const linkedDoc = (
  legs: ReadonlyArray<Flows.Leg>,
  projected: ReadonlyArray<string> = LINKED_HEADERS
): Flows.FlowsDoc => {
  const named = new Set(projected.map((h) => h.toLowerCase()))
  return {
    ...doc(
      legs.map((l) => ({
        ...l,
        msgs: l.msgs.map((m) => ({ ...m, headers: (m.headers ?? []).filter((h) => named.has(h.name.toLowerCase())) }))
      })),
      legs.map((_, i) => ({ legs: [i], initial_invite: { leg: i, msg: 0 } }))
    ),
    emit_headers: [...projected]
  }
}

/**
 * A serial hunt whose attempts carry Call-IDs of their own: the caller's leg
 * (leg 0, answered by the SUT), a first attempt to one callee answered 486
 * (leg 1), then a second to another callee after it, answered 200 and torn
 * down with the caller's dialog (leg 2). Each attempt's INVITE names the
 * caller leg's Call-ID in `header`, which the document projects. Each leg is
 * its own call group.
 */
export const linkedHuntFlows = (header: string = LINK_HEADER): Flows.FlowsDoc => {
  const caller = "caller-1@192.0.2.9"
  const [first, second] = ["attempt-1@192.0.2.1", "attempt-2@192.0.2.1"]
  const [firstCallee, secondCallee] = ["sip:+15556000004@192.0.2.2", "sip:+15556000005@192.0.2.3"]
  const { caller: a, sut, callee, other } = SOCKETS
  const link = { headers: [`${header}: ${caller}`] }
  const projected = (msg: Flows.Msg): Flows.Msg => ({ ...msg, headers: [{ name: header, value: caller }] })
  return {
    ...doc(
      [
        leg(caller, oneHop(a, sut), [
          request({ callId: caller, seq: 1, method: "INVITE", src: a, dst: sut, ts_ms: 0 }),
          response({ callId: caller, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: a, ts_ms: 5 }),
          response({ callId: caller, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: a, ts_ms: 1_010, toTag: "sut-tag" }),
          request({ callId: caller, seq: 1, method: "ACK", src: a, dst: sut, ts_ms: 1_015, toTag: "sut-tag" }),
          request({ callId: caller, seq: 2, method: "BYE", src: a, dst: sut, ts_ms: 5_000, toTag: "sut-tag" }),
          response({ callId: caller, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: a, ts_ms: 5_005, toTag: "sut-tag" })
        ]),
        leg(first, oneHop(sut, callee), [
          projected(request({ callId: first, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10, ruri: firstCallee, ...link })),
          response({ callId: first, seq: 1, status: 486, reason: "Busy Here", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 400, toTag: "first-tag", toUri: firstCallee }),
          request({ callId: first, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 405, toTag: "first-tag", ruri: firstCallee })
        ]),
        leg(second, oneHop(sut, other), [
          projected(request({ callId: second, seq: 1, method: "INVITE", src: sut, dst: other, ts_ms: 500, ruri: secondCallee, ...link })),
          response({ callId: second, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: other, dst: sut, ts_ms: 1_000, toTag: "second-tag", toUri: secondCallee }),
          request({ callId: second, seq: 1, method: "ACK", src: sut, dst: other, ts_ms: 1_005, toTag: "second-tag", ruri: secondCallee }),
          request({ callId: second, seq: 2, method: "BYE", src: sut, dst: other, ts_ms: 5_010, toTag: "second-tag", ruri: secondCallee }),
          response({ callId: second, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: other, dst: sut, ts_ms: 5_015, toTag: "second-tag", toUri: secondCallee })
        ])
      ],
      [
        { legs: [0], initial_invite: { leg: 0, msg: 0 }, final_status: 200, final_us: 1_010_000 },
        { legs: [1], t0_us: 10_000, initial_invite: { leg: 1, msg: 0 }, final_status: 486, final_us: 400_000 },
        { legs: [2], t0_us: 500_000, initial_invite: { leg: 2, msg: 0 }, final_status: 200, final_us: 1_000_000 }
      ]
    ),
    emit_headers: [header]
  }
}

/**
 * `n` answered calls, 20 s apart, each a caller leg and a callee leg naming it
 * by `rule`'s headers: the caller leg's Call-ID where its left header is
 * `Call-ID`, else the call's number in both headers.
 */
export const linkedCallsFlows = (n: number, rule: Rules.HeaderPairRule = LINK_RULE): Flows.FlowsDoc =>
  linkedDoc(
    Array.from({ length: n }, (_, i) => {
      const caller = `caller-${i}@192.0.2.9`
      const byCallId = Flows.isCallIdHeader(rule.left.header)
      const key = byCallId ? caller : String(i)
      return [
        callerLeg(caller, i * 20_000, byCallId ? [] : [[rule.left.header, key]]),
        dialledLeg(`callee-${i}@192.0.2.1`, i * 20_000 + 10, [[rule.right.header, key]])
      ]
    }).flat()
  )

/**
 * `rule`, counting how often a side's pattern is read, once per side each time
 * the rule is compiled for a correlation, and how often a side's header is
 * read, once per leg per side as the rule reads that leg's INVITE, and once
 * per side to check the document projects it.
 */
export const countingRule = (rule: Rules.HeaderPairRule) => {
  let patterns = 0
  let headers = 0
  const side = (s: Rules.HeaderSide): Rules.HeaderSide => ({
    get header() {
      headers += 1
      return s.header
    },
    get pattern() {
      patterns += 1
      return s.pattern
    }
  })
  return { rule: { ...rule, left: side(rule.left), right: side(rule.right) }, patterns: () => patterns, headers: () => headers }
}
