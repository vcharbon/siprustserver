/**
 * The `capability` section: the advertisements in which a sender states its
 * own capabilities (ADVERTISEMENT), where the replayed stack's set and the
 * capture's need not agree, and those the capture carried where RFC 3261
 * §20 marks them mandatory or not applicable.
 */
import { delta, is, itemsOf, itemsRemoved, onlyReplayed } from "@sip/delta-rules/header-items"
import { allHanded, drivenIn } from "@sip/delta-rules/provenance"
import { isRequest, statusOf } from "@sip/delta-rules/scope"
import type { Probe } from "@sip/pipeline"
import { ADVERTISEMENT } from "./header-lists.js"
import type { RfcRow } from "./row.js"

/** An advertisement handed in on a `method` request, driven, with items the replay removed. */
const advertisementDroppedOn = (h: Probe.HeaderProbe, method: string): boolean =>
  is(h, ...ADVERTISEMENT) && isRequest(h.scope, method) && itemsRemoved(h) && h.inbound && drivenIn(h)

export const capabilitySetRelayedPastRelease: RfcRow = {
  id: "capability-set-relayed-past-release",
  on: { kind: "header", family: ADVERTISEMENT },
  gate: "releasedLegSteps",
  rfc: "RFC 3261 §15.1.2",
  why:
    "An advertisement on a response to a transaction still open when its leg released: RFC 3261 " +
    "§15.1.2 leaves its answer to the stack, the releasing side's own or the peer's through the " +
    "relay, and the replay carries the peer's lines. Different messages, not different sets.",
  header: (h, facts) =>
    h.scope.kind === "response" && facts.releasedLegSteps.has(h.step) && onlyReplayed(h) && h.inbound &&
    drivenIn(h) && h.inboundValues.length > 0 && allHanded(h, itemsOf(h, "replayed"))
}

export const methodNotAllowedStatesAllow: RfcRow = {
  id: "method-not-allowed-states-allow",
  on: { kind: "header", family: ["allow"] },
  rfc: "RFC 3261 §8.2.1",
  why:
    "An Allow the replay states on a 405 where the capture's carries none: RFC 3261 §8.2.1 makes " +
    "Allow a MUST on a 405, which the captured 405 broke, and the replay's names the methods its " +
    "sender supports.",
  header: (h) => statusOf(h.scope) === 405 && onlyReplayed(h)
}

export const failureFinalAllowEventsRelayed: RfcRow = {
  id: "failure-final-allow-events-relayed",
  on: { kind: "header", family: ["allow-events"], scope: ["INVITE"] },
  rfc: "RFC 6665 §8.2.2",
  why:
    "The callee's Allow-Events the replay carries on a failure final to the INVITE where the " +
    "capture's final dropped it, every replayed item one a peer handed in: the event packages the " +
    "callee states it supports (RFC 6665 §8.2.2), relayed with its failure.",
  header: (h) => {
    const status = statusOf(h.scope, "INVITE")
    return status !== undefined && status >= 300 && onlyReplayed(h) && allHanded(h, itemsOf(h, "replayed"))
  }
}

export const capabilitySetAddedByStack: RfcRow = {
  id: "capability-set-added-by-stack",
  on: { kind: "header", family: ADVERTISEMENT },
  rfc: "RFC 3261 §20.1, §20.5, §20.37",
  why:
    "The replay advertises more than the capture and drops nothing: a set widened or minted. An " +
    "advertisement states its sender's own capabilities (RFC 3261 §20.1, §20.5, §20.37), so a " +
    "stack answering in its own name may state more than the captured one.",
  header: (h) => {
    const d = delta(h)
    return d !== undefined && d.added.length > 0 && d.removed.length === 0
  }
}

export const locallyAnsweredNonInviteAdvertisement: RfcRow = {
  id: "locally-answered-non-invite-advertisement",
  on: { kind: "header", family: ["accept"], scope: ["BYE"] },
  rfc: "RFC 3261 §20, §20.1",
  why:
    "An Accept the capture carried on a 200 to a BYE that the replay states nowhere: RFC 3261 §20 " +
    "Table 2 marks Accept not applicable in a 2xx to a BYE (§20.1), which the captured side broke " +
    "and the replay keeps.",
  header: (h) => statusOf(h.scope, "BYE") === 200 && itemsOf(h, "replayed").length === 0
}

export const capabilitySetInertOnTransactionDerived: RfcRow = {
  id: "capability-set-inert-on-transaction-derived",
  on: {
    kind: "header",
    family: ["allow", "supported", "accept", "accept-encoding", "accept-language"],
    scope: ["ACK"]
  },
  rfc: "RFC 3261 §17.1.1.3, §20.5",
  why:
    "An Allow, Supported or Accept family header handed to the system on an ACK and not relayed: " +
    "RFC 3261 §20 Tables 2 and 3 mark each not applicable on an ACK, which the captured side broke " +
    "in relaying it. Allow-Events is outside those tables; a line the replay places on its ACK " +
    "stays visible.",
  header: (h) => itemsOf(h, "replayed").length === 0 && advertisementDroppedOn(h, "ACK")
}

export const capabilitySetNotGenerated: RfcRow = {
  id: "capability-set-not-generated",
  on: { kind: "header", family: ADVERTISEMENT },
  rfc: "RFC 3261 §20.1, §20.5, §20.37",
  why:
    "Advertised tokens removed that never reached the system: no relay can produce a token it " +
    "never received, and an advertisement states the sender's own capability set (RFC 3261 " +
    "§20.1, §20.5, §20.37).",
  header: (h) => itemsRemoved(h) && !h.inbound
}
