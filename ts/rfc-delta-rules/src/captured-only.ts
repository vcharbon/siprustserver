/**
 * The `captured-only` section: a header the capture carried and the replay
 * does not, where no relay could have carried it (the captured side minted it,
 * or the sender's own clock stated it) or where the run never drove its input
 * in.
 */
import { carried } from "@sip/delta-rules/families"
import { itemsOf, onlyCaptured } from "@sip/delta-rules/header-items"
import { glareRejectionLost, lostWereHanded, undrivenLoss } from "@sip/delta-rules/provenance"
import { statusOf } from "@sip/delta-rules/scope"
import { descriptorForm, timestampOf } from "@sip/delta-rules/value-forms"
import type { RfcFacts } from "./facts.js"
import { BODY_METADATA, IDENTITY_ASSERTION } from "./header-lists.js"
import type { RfcRow } from "./row.js"

export const selfDateRegenerated: RfcRow = {
  id: "self-date-regenerated",
  on: { kind: "header", family: ["date"] },
  rfc: "RFC 3261 §20.17",
  why:
    "RFC 3261 §20.17 makes Date the time the message is first sent, the sender's own clock " +
    "reading; relaying the capture's would assert a time the replay is not sending at. Every " +
    "scope.",
  header: (h) => onlyCaptured(h)
}

export const selfTimestampRegenerated: RfcRow = {
  id: "self-timestamp-regenerated",
  on: { kind: "header", family: ["timestamp"] },
  rfc: "RFC 3261 §20.38",
  why:
    "A Timestamp the capture carried on a request and the replay states none of: RFC 3261 §20.38 " +
    "defines it as when the UAC sent the request, and a relayed request whose source carried " +
    "one states the stack's own reading. Requests only.",
  header: (h) => onlyCaptured(h) && h.scope.kind !== "response"
}

export const mintedFinalTimestampEchoed: RfcRow = {
  id: "minted-final-timestamp-echoed",
  on: { kind: "header", family: ["timestamp"] },
  rfc: "RFC 3261 §8.2.6.1, §20 Table 3",
  why:
    "A Timestamp the capture states on a final the captured side minted itself, byte for byte the " +
    "request's with no peer's delayed echo of it inbound, and the replay states none: RFC 3261 " +
    "§20 Table 3 makes the echo optional past the 100 Trying.",
  header: (h) => {
    const status = statusOf(h.scope)
    if (!onlyCaptured(h) || status === undefined || status < 200) return false
    const authoredFailureOrUndriven = status >= 300 || h.driven === undefined
    const inbound = h.inboundValues.map((v) => v.trim())
    const peerEchoed = (item: string) => inbound.some((v) => v !== item && timestampOf(v) === timestampOf(item))
    const requestReading = (item: string) => h.driven === undefined || item.split(/\s+/).length === 1
    return authoredFailureOrUndriven && itemsOf(h, "captured").every((item) =>
      inbound.includes(item) && !peerEchoed(item) && requestReading(item)
    )
  }
}

export const mintedGlareRejectionRelaysNothing: RfcRow = {
  id: "minted-glare-rejection-relays-nothing",
  on: { kind: "header", scope: ["INVITE"] },
  rfc: "RFC 3261 §14.2, §20.33",
  why:
    "A header missing from a 491 to INVITE the replay minted (no relay input identified) where the " +
    "capture relayed the peer's 491 with it: RFC 3261 §14.2 obliges a 491 against an INVITE in " +
    "flight, and the race went the other way on each side.",
  header: (h) => glareRejectionLost(h) && h.driven === undefined && h.inbound
}

export const glareRejectionRetryAfterNotGenerated: RfcRow = {
  id: "glare-rejection-retry-after-not-generated",
  on: { kind: "header", family: ["retry-after"], scope: ["INVITE"] },
  rfc: "RFC 3261 §14.1, §20.33",
  why:
    "A Retry-After the capture stated on a 491 to INVITE its side minted and the replay lacks: " +
    "RFC 3261 §20.33 does not list 491 and §14.1 puts the back-off in the UAC, so no MUST " +
    "decides it.",
  header: (h) => glareRejectionLost(h) && !h.inbound
}

export const [accessNetworkContextNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(
  ["p-charging-function-addresses", "p-access-network-info", "p-visited-network-id"],
  {
    generated: {
      id: "access-network-context-not-generated",
      rfc: "RFC 7315 §4.3, §4.4, §4.5",
      why:
        "P-Access-Network-Info, P-Charging-Function-Addresses and P-Visited-Network-ID the capture " +
        "minted: RFC 7315 §4.4, §4.5 and §4.3 make them the UA's access network, the operator's " +
        "charging function (CCF/ECF) addresses and the visited network, none a B2BUA's to state."
    }
  }
)

export const [identityAssertionNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(IDENTITY_ASSERTION, {
  generated: {
    id: "identity-assertion-not-generated",
    rfc: "RFC 3325 §5",
    why:
      "RFC 3325 §5 lets only an element that has authenticated the user insert the assertion. A " +
      "B2BUA under replay authenticates nobody, so minting one it never received would assert an " +
      "identity on no evidence."
  }
})

export const [privacyInstructionNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(["privacy"], {
  generated: {
    id: "privacy-instruction-not-generated",
    rfc: "RFC 3323 §4.2",
    why:
      "RFC 3323 §4.2 makes the priv-value the user's request; a Privacy header the system never " +
      "received states a level nobody asked for."
  }
})

export const [releaseCauseNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(["reason"], {
  generated: {
    id: "release-cause-not-generated",
    rfc: "RFC 3326 §2",
    why:
      "A Reason the capture stated on a release its side originated where the replay states none. " +
      "RFC 3326 §2 makes Reason the sender's statement, so no relay produces it."
  }
})

export const bodyDescriptorDroppedWithItsBody: RfcRow = {
  id: "body-descriptor-dropped-with-its-body",
  on: { kind: "header", family: BODY_METADATA },
  rfc: "RFC 3261 §20.11–§20.13, §20.24",
  why:
    "A body descriptor the relay drops on a message that carries no body on either side, so it " +
    "describes no octets; or one the capture left on a multipart that the replay states, value " +
    "for value, on the part it describes (RFC 3261 §20.11).",
  header: (h) => {
    const inParts = new Set(h.inReplayedParts.map(descriptorForm))
    return onlyCaptured(h) && (h.bodiless || itemsOf(h, "captured").every((v) => inParts.has(descriptorForm(v))))
  }
}

export const [bodyMetadataNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(BODY_METADATA, {
  generated: {
    id: "body-metadata-not-generated",
    rfc: "RFC 3261 §20.11–§20.13, §20.24",
    why:
      "Body metadata the capture minted and the replay does not: a descriptor is its body author's " +
      "statement (RFC 3261 §20.11–§20.13, §20.24)."
  }
})

export const [invitationExpiryNotGenerated]: ReadonlyArray<RfcRow> = carried<RfcFacts>(["expires"], {
  generated: {
    id: "invitation-expiry-not-generated",
    rfc: "RFC 3261 §13.2.1, §20.19",
    why:
      "An Expires the capture stated on an INVITE its side minted while no message carried one as " +
      "a header field (RFC 3261 §19.1.5): §13.2.1 lets the UAC add one to bound the invitation, so " +
      "the bound is its sender's choice and the replay need not state it."
  }
}, "INVITE")

export const relayInputNotDriven: RfcRow = {
  id: "relay-input-not-driven",
  on: { kind: "header" },
  rfc: "ADR-0024",
  why:
    "A header the capture shows the system relaying that the run never drove in: a message the " +
    "replay composes as a stack automatic (ADR-0024) sends identifying headers only, so the loss " +
    "is the replay's. Every lost item is one the system was handed; once driven in, the relay " +
    "rows judge it.",
  header: (h) => undrivenLoss(h) && lostWereHanded(h)
}
