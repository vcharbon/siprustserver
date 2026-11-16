/**
 * The values an RFC makes the sending stack's own (ADR-0062 item 5): a
 * difference each row names arises on any B2BUA replay of any capture. Every
 * row cites its clause in `why` and `rfc`, carries no section and no lane note,
 * and keys on a header family no other row here names, so no two rows match
 * one probe. A deployment places each row by name in its own match order.
 */
import { bothSides, itemsOf, lengthStatesBody, singles } from "@sip/delta-rules/header-items"
import { addrPair, paramsButTag, sameParams, urisEqual } from "@sip/delta-rules/identity"
import type { RuleSpec } from "@sip/delta-rules/rule"
import { statusOf } from "@sip/delta-rules/scope"
import { inStatuses } from "@sip/delta-rules/shapes"
import { cseqMethodOf, mediaTypeApartFromBoundary, rackParts } from "@sip/delta-rules/value-forms"
import type { Probe } from "@sip/pipeline"
import type { RfcFacts } from "./facts.js"

/** An RFC row, over the facts {@link RfcFacts} names. */
export type RfcRow = RuleSpec<RfcFacts>

/** The replay carries the header: a message it sent without one stays a finding. */
const replayCarries = (h: Probe.HeaderProbe): boolean => itemsOf(h, "replayed").length > 0

export const viaStackOwned: RfcRow = {
  id: "via-stack-owned",
  on: { kind: "header", family: ["via"] },
  rfc: "RFC 3261 §8.1.1.7",
  why:
    "The replay stack builds its own Via and branch; the captured hop chain cannot survive a " +
    "different topology. A message the replay sent with no Via stays a finding (RFC 3261 §8.1.1.7).",
  header: replayCarries
}

export const callIdStackOwned: RfcRow = {
  id: "call-id-stack-owned",
  on: { kind: "header", family: ["call-id"] },
  rfc: "RFC 3261 §8.1.1.4",
  why:
    "The replay allocates a fresh Call-ID per run; captured values would collide across runs. " +
    "A message the replay sent with no Call-ID stays a finding (RFC 3261 §8.1.1.4).",
  header: replayCarries
}

export const contentLengthStackOwned: RfcRow = {
  id: "content-length-stack-owned",
  on: { kind: "header", family: ["content-length"] },
  rfc: "RFC 3261 §20.14",
  why:
    "A Content-Length whose two sides agree on whether a body is carried (an absent length " +
    "reads 0): it is recomputed from the rendered body (RFC 3261 §20.14), whose own difference " +
    "is its body row. A body one side carries and the other does not stays visible.",
  header: (h) => {
    const captured = lengthStatesBody(h, "captured")
    return captured !== undefined && captured === lengthStatesBody(h, "replayed")
  }
}

export const multipartBoundaryStackOwned: RfcRow = {
  id: "multipart-boundary-stack-owned",
  on: { kind: "header", family: ["content-type"] },
  rfc: "RFC 2046 §5.1.1",
  why:
    "A multipart Content-Type whose sides name the same media type and parameters apart from " +
    "`boundary`: the boundary is the composing agent's choice (RFC 2046 §5.1.1) and the parts " +
    "it frames are compared by the body row. Any other parameter or type stays visible.",
  header: (h) => {
    const pair = singles(h)
    if (pair === undefined) return false
    const left = mediaTypeApartFromBoundary(pair[0])
    const right = mediaTypeApartFromBoundary(pair[1])
    return left.type.startsWith("multipart/") && left.type === right.type && left.params === right.params
  }
}

export const maxForwardsStackOwned: RfcRow = {
  id: "max-forwards-stack-owned",
  on: { kind: "header", family: ["max-forwards"] },
  rfc: "RFC 3261 §8.1.1.6",
  why:
    "Decremented per hop; the replay topology has a different hop count than the captured one. " +
    "A request the replay sent with no Max-Forwards stays a finding (RFC 3261 §8.1.1.6).",
  header: (h) => h.scope.kind === "response" || replayCarries(h)
}

export const cseqNumberStackOwned: RfcRow = {
  id: "cseq-number-stack-owned",
  on: { kind: "header", family: ["cseq"] },
  rfc: "RFC 3261 §8.1.1.5, §12.2.1.1",
  why:
    "The stack owns dialog CSeq numbering (RFC 3261 §8.1.1.5, §12.2.1.1), so a CSeq whose " +
    "method matches on both sides and whose number differs is a renumbering. A method change " +
    "stays visible.",
  header: (h) => {
    const pair = singles(h)
    const method = pair === undefined ? undefined : cseqMethodOf(pair[0])
    return method !== undefined && method === cseqMethodOf(pair![1])
  }
}

export const rseqStackOwned: RfcRow = {
  id: "rseq-stack-owned",
  on: { kind: "header", family: ["rseq"] },
  rfc: "RFC 3262 §7.1",
  why:
    "An RSeq both sides carry with a different number: RFC 3262 §7.1 numbers the reliable " +
    "provisionals of one transaction, and the a-leg transaction is the stack's, which mints " +
    "its own sequence and translates the RAck back. An RSeq the replay omits stays visible.",
  header: (h) => inStatuses(statusOf(h.scope), "1xx") && bothSides(h)
}

export const rackCseqStackOwned: RfcRow = {
  id: "rack-cseq-stack-owned",
  on: { kind: "header", family: ["rack"] },
  rfc: "RFC 3262 §7.2",
  why:
    "An RAck whose method matches and whose RSeq acknowledges the replay's own provisional, or " +
    "the one the leg's actor renumbered: RFC 3262 §7.2 composes it from the provisional's RSeq " +
    "and the stack's own CSeq. A PRACK of any other provisional stays visible.",
  header: (h, facts) => {
    const pair = singles(h)
    const a = pair === undefined ? undefined : rackParts(pair[0])
    const b = pair === undefined ? undefined : rackParts(pair[1])
    if (a === undefined || b === undefined || a.method !== b.method) return false
    const acknowledged = Number(b.rseq)
    return Number(a.rseq) === acknowledged || facts.acknowledgedRSeqAtStep.get(h.step) === acknowledged
  }
}

export const fromToTagStackOwned: RfcRow = {
  id: "from-to-tag-stack-owned",
  on: { kind: "header", family: ["from", "to"] },
  rfc: "RFC 3261 §19.3, §19.1.4",
  why:
    "Dialog tags are allocated per run (RFC 3261 §19.3); the address either side of the tag is " +
    "still compared (§19.1.4), every URI part with it: scheme, user, host, port, URI and user " +
    "parameters, and `?headers`.",
  header: (h) => {
    const pair = singles(h)
    const parsed = addrPair(h)
    if (pair === undefined || parsed === undefined) return false
    const [a, b] = parsed
    return a.params.get("tag") !== b.params.get("tag") && a.display === b.display &&
      sameParams(paramsButTag(a.params), paramsButTag(b.params)) && urisEqual(pair[0], pair[1])
  }
}

/** Every RFC row, each once. Their order decides nothing: no two match one probe. */
export const RFC_ROWS: ReadonlyArray<RfcRow> = [
  viaStackOwned,
  callIdStackOwned,
  contentLengthStackOwned,
  multipartBoundaryStackOwned,
  maxForwardsStackOwned,
  cseqNumberStackOwned,
  rseqStackOwned,
  rackCseqStackOwned,
  fromToTagStackOwned
]
