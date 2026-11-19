/**
 * The `stack-owned` section: the values an RFC makes the sending stack's own,
 * which differ on any B2BUA replay of any capture (ADR-0062 item 5), the
 * stack's own Contact, the dialog's own addresses as the stack keeps them,
 * and the dialog's session description where the captured side broke its
 * continuity.
 */
import { bothSides, canon, itemsOf, lengthStatesBody, onlyReplayed, single, singles } from "@sip/delta-rules/header-items"
import { addrPair, paramsButTag, sameAddressButTag, sameParams, uriReplaced, urisEqual } from "@sip/delta-rules/identity"
import { openedBefore, openerValue, sentBefore } from "@sip/delta-rules/run-facts"
import { inDialogRequest, isRequest, onLeg, statusOf } from "@sip/delta-rules/scope"
import { inStatuses } from "@sip/delta-rules/shapes"
import {
  cseqMethodOf,
  mediaTypeApartFromBoundary,
  originLine,
  rackParts,
  sessIdAlone,
  sessIdOf
} from "@sip/delta-rules/value-forms"
import type { Probe } from "@sip/pipeline"
import type { RfcRow } from "./row.js"

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

export const contactHostIsReplaySocket: RfcRow = {
  id: "contact-host-is-replay-socket",
  on: { kind: "header", family: ["contact"] },
  rfc: "RFC 3261 §8.1.1.8",
  why:
    "The Contact host:port is the socket the replay bound, the user part held: RFC 3261 §8.1.1.8 " +
    "makes the Contact the sending instance's own address. A 3xx's Contacts are redirect targets, " +
    "not the stack's, and stay findings.",
  header: (h) => {
    const pair = addrPair(h)
    if (pair === undefined || inStatuses(statusOf(h.scope), "3xx")) return false
    const [a, b] = pair
    return a.uri.user === b.uri.user && a.uri.host !== b.uri.host
  }
}

export const contactAdvertisedOnLocallyAnswered: RfcRow = {
  id: "contact-advertised-on-locally-answered",
  on: { kind: "header", family: ["contact"], scope: ["BYE", "CANCEL", "INVITE", "ACK"] },
  rfc: "RFC 3261 §20.10",
  why:
    "A Contact the stack states, and the capture did not, on a message it answers in its own " +
    "right: a 2xx to BYE or CANCEL, a 4xx-6xx INVITE final, an ACK. RFC 3261 §20.10 neither " +
    "requires nor forbids it there; a 3xx names redirect targets and stays out.",
  header: (h) => {
    if (!onlyReplayed(h)) return false
    if (h.scope.kind !== "response") return isRequest(h.scope, "ACK")
    const { cseqMethod, status } = h.scope
    if (inStatuses(status, "2xx")) return cseqMethod === "BYE" || cseqMethod === "CANCEL"
    return status >= 400 && cseqMethod === "INVITE"
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

export const dialogDisplayNameDropped: RfcRow = {
  id: "dialog-display-name-dropped",
  on: { kind: "header", family: ["from", "to"] },
  rfc: "RFC 3261 §12.2.1.1",
  why:
    "On an in-dialog request, a From/To whose display name differs from the one the capture " +
    "shows: RFC 3261 §12.2.1.1 builds both from the dialog's own addresses, which carry the " +
    "display name its creating INVITE stated, on every leg role.",
  header: (h) => {
    const pair = addrPair(h)
    return inDialogRequest(h.scope) && pair !== undefined && pair[0].display !== pair[1].display
  }
}

export const callerDialogAddressKept: RfcRow = {
  id: "caller-dialog-address-kept",
  on: { kind: "header", family: ["from", "to"] },
  rfc: "RFC 3261 §12.2.1.1",
  why:
    "On an in-dialog request toward the caller, a From/To whose URI the capture replaced where " +
    "the replay states her own address from the INVITE the run sent, every part but the tag: " +
    "RFC 3261 §12.2.1.1 sets both URIs from the dialog state, which the captured side broke.",
  header: (h, { facts }) => {
    const replayed = single(h, "replayed")
    const opened = openerValue(facts, h.step, canon(h.name) === "to" ? "from" : "to")
    return onLeg(h, ["caller"]) && inDialogRequest(h.scope) && uriReplaced(h) &&
      sentBefore(facts, h.step).some((s) => s.method === "INVITE") &&
      replayed !== undefined && opened !== undefined && sameAddressButTag(opened, replayed)
  }
}

export const sdpOriginStackOwned: RfcRow = {
  id: "sdp-origin-stack-owned",
  on: { kind: "body" },
  rfc: "RFC 4566 §5.2",
  why:
    "The `o=` sess-id and sess-version of a session the replayed system opens (an offer it " +
    "composes and the descriptions after it): RFC 4566 §5.2 leaves both to the originator. Marked " +
    "only where nobody drove that origin in and each version step matches the capture's.",
  body: (b) => b.sdp?.line === "o=" && b.sdp.mintedOrigin === true
}

export const sdpSessionIdRestated: RfcRow = {
  id: "sdp-session-id-restated",
  on: { kind: "body" },
  rfc: "RFC 3264 §8",
  why:
    "An in-dialog description whose origin differs in the sess-id alone, the replay's the one " +
    "the leg's session opened with: a peer changed its sess-id mid-dialog, which RFC 3264 §8 " +
    "forbids, the capture relayed it, and the replay restates the leg's session.",
  body: (b, { facts }) => {
    const first = openedBefore(facts, b.step)?.origin
    if (b.sdp === undefined || !inDialogRequest(b.scope) || first === undefined) return false
    const [captured = "", replayed = ""] = [b.captured[0], b.replayed[0]]
    const restated = (left: string, right: string): boolean =>
      sessIdAlone(left, right) && sessIdOf(right) === sessIdOf(first) && sessIdOf(left) !== sessIdOf(first)
    if (b.sdp.section === "session" && b.sdp.line === "o=") return restated(captured, replayed)
    if (b.sdp.section !== "document" || b.sdp.line !== "bytes") return false
    const left = originLine(captured)
    const right = originLine(replayed)
    return left !== undefined && right !== undefined && restated(left, right) && b.sdp.orderOnly === true
  }
}

export const sdpVersionStepOne: RfcRow = {
  id: "sdp-version-step-one",
  on: { kind: "body" },
  rfc: "RFC 3264 §8",
  why:
    "The capture's sess-version steps by more than one on this leg, which RFC 3264 §8 forbids, " +
    "where the replay's steps by one or repeats over an unchanged description, every later one " +
    "as far from the capture's. A replay that jumps, or exceeds the capture's, stays visible.",
  body: (b) =>
    b.sdp?.versionStepOne === true && (b.sdp.line === "o=" || (b.sdp.line === "bytes" && b.sdp.orderOnly === true))
}
