/**
 * The `keepalive` section: the headers of the in-dialog OPTIONS keepalive
 * exchange, an exchange the answering stack terminates (RFC 3261 §11.2), so
 * neither side's headers on it ride a relay path to the other's.
 */
import { keepalive } from "@sip/delta-rules/families"
import { is, itemsOf, onlyCaptured } from "@sip/delta-rules/header-items"
import { allHanded } from "@sip/delta-rules/provenance"
import type { RfcFacts } from "./facts.js"
import { ADVERTISEMENT, PRODUCT_IDENTITY, STACK_COMPOSED } from "./header-lists.js"
import type { RfcRow } from "./row.js"

export const [
  keepaliveExchangeLocallyTerminated,
  keepaliveExchangeRelayedEndToEnd,
  keepaliveExchangeAdvertisement,
  keepaliveExchangeChargingVector,
  keepaliveExchangeHeaderStated
]: ReadonlyArray<RfcRow> = keepalive<RfcFacts>([
  {
    id: "keepalive-exchange-locally-terminated",
    family: STACK_COMPOSED,
    why:
      "A header the stack composes from its own state (STACK_COMPOSED) on the in-dialog OPTIONS " +
      "keepalive exchange: RFC 3261 §11.2 has the receiving hop answer OPTIONS itself, so these " +
      "describe the answering stack and no relay path joins the captured exchange to the replay's."
  },
  {
    id: "keepalive-exchange-relayed-end-to-end",
    family: [...ADVERTISEMENT, "p-charging-vector"],
    why:
      "An advertisement or P-Charging-Vector the capture carried on the in-dialog OPTIONS exchange " +
      "and the replay does not, every captured item one a peer handed in: the captured side relayed " +
      "a peer's OPTIONS or its answer end to end, where the replay's stack answered it itself (RFC 3261 §11.2).",
    also: (h) => onlyCaptured(h) && allHanded(h, itemsOf(h, "captured"))
  },
  {
    id: "keepalive-exchange-advertisement",
    family: ADVERTISEMENT,
    why:
      "An Allow / Supported / Accept* on the in-dialog OPTIONS keepalive exchange: the set the " +
      "answering stack states for itself on a request it originates or answers locally (RFC 3261 " +
      "§11.2), which no relay carries."
  },
  {
    id: "keepalive-exchange-charging-vector",
    family: ["p-charging-vector"],
    why:
      "A P-Charging-Vector on the in-dialog OPTIONS keepalive exchange, either side, either " +
      "direction: the exchange is the answering stack's own (RFC 3261 §11.2)."
  },
  {
    id: "keepalive-exchange-header-stated",
    why:
      "Any header on the in-dialog OPTIONS keepalive exchange but a product banner and a Date: what " +
      "either stack states on an exchange it answers itself (RFC 3261 §11.2), the headers a " +
      "deployment adds to every message of the call among them.",
    also: (h) => !is(h, ...PRODUCT_IDENTITY, "date")
  }
])
