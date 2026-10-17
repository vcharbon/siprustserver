# The front proxy trusts by ingress face

**Status:** accepted (2026-10-12)

**Owner:** `crates/sip-proxy/src/core/request/classify.rs` (a request's
direction), `core/request/admission.rs` (the To-tag rule and the gate),
`core/response.rs` (the response rule), `crates/sip-message/src/header/via.rs`
(`received`), `crates/sip-proxy-runner/src/config.rs` (the single-face warning,
the wildcard refusal), `deploy/helm/siprust/templates/masq-exempt.yaml` and
`files/masq-exempt.sh` (the edge node's drop rule).

## Context

The proxy relayed a request out of its external face to the Request-URI
whenever its sender claimed to be a worker: an `;outbound` marker on the
proxy's own Route entry, a top Via sent-by naming a worker, or a source that is
one. None of the three is authenticated. The cookie's MAC does not cover the
marker, and the sender writes its own Via. A foreign loose Route was followed
too, ahead of worker selection. Such a request reached a host the sender chose
with no worker on the path, no routing decision and no record, and a new INVITE
skipped the admission gate. Three related gaps sat on the same path. A response
whose top Via named the proxy was relayed to whatever its second Via named. A
`received` the sender wrote on its own Via was kept. An INVITE with a forged
To-tag skipped the gate. The recv loop knew which face each datagram arrived on
but used it only for a metric.

## Decision

1. **The ingress face is the trust.** The recv loop hands the face a datagram
   arrived on to request and response handling. The internal face is the worker
   plane and is trusted. The external face is not. This holds only while the
   internal socket is reachable from the internal plane alone:
   - a dual-face proxy refuses to boot when `PROXY_LISTEN` or
     `PROXY_LISTEN_EXT` is a wildcard address, since a wildcard bind delivers
     datagrams to other local addresses (an edge node's external-interface
     address among them) to that socket. Test:
     `config::tests::a_dual_face_wildcard_bind_refuses_the_boot`;
   - on an edge node both addresses live on one host, so a caller-plane host
     that routes the internal address through the node would reach the
     internal socket. The deployment drops every packet to `PROXY_VIP` that
     arrives on the external interface (`deploy/helm/siprust/files/masq-exempt.sh`,
     re-asserted with its other rules).
2. **On the internal face a request is worker-outbound** when its direction
   entry carries `;outbound`, when its source is a registry worker, or when its
   top Via sent-by is one. The marker is what recognises a worker rebooted onto
   a pod address the registry does not hold. A loose Route past the proxy's own
   is followed: a worker's in-dialog route set holds the far side's
   Record-Route entries. Tests:
   `ingress_trust::a_registered_worker_on_the_internal_face_is_worker_outbound`,
   `ingress_trust::an_outbound_marker_on_the_internal_face_is_worker_outbound_from_an_unlisted_worker`.
3. **On the external face no claim of the internal plane holds.** A request is
   never worker-outbound there: the marker, a worker Via and a worker source are
   ignored. It is routed as an inbound request, by its cookie or by a fresh
   selection, and a new call meets the gate. Every Route entry past the proxy's
   own is removed. Nothing is refused for carrying a claim. Each claim is
   counted in `sip_proxy_external_untrusted_total{kind}`. Tests:
   `ingress_trust::an_outbound_marker_on_the_external_face_is_ignored`,
   `ingress_trust::a_worker_via_on_the_external_face_is_ignored`,
   `ingress_trust::a_foreign_loose_route_on_the_external_face_is_removed`,
   `worker_outbound_tests::an_unsigned_outbound_marker_is_trusted_only_on_the_internal_face`.
4. **A response arriving on the external face is relayed only to a worker.**
   It goes to its second Via's response address (`received`/`rport` over the
   sent-by, RFC 3261 §18.2.2) when that is a registry worker, a departed one
   included while its tombstone lasts. Otherwise, when that Via's sent-by is a
   registry worker, it goes to the sent-by: the worker's registry address,
   reachable from the proxy, which carries a worker whose source address was
   translated on its way in. Any other response is dropped and counted
   (`response_not_to_worker`). Every destination is a worker; a sender-written
   `received` never steers one elsewhere. A response arriving on the internal
   face follows its Via. Tests:
   `ingress_trust::a_response_on_the_external_face_whose_next_via_is_no_worker_is_dropped`,
   `ingress_trust::a_response_naming_a_worker_with_a_foreign_received_reflects_nothing`,
   `ingress_trust::the_answer_to_a_translated_worker_reaches_it_on_the_external_face`.
5. **An INVITE with a To-tag that no verified cookie backs is a new call on the
   external face.** It meets the gate, and the strategy admits it under every
   new-call cap (`SelectOpts::admit_as_new`). It is never refused for its
   To-tag alone. Tests:
   `ingress_trust::a_forged_to_tag_invite_on_the_external_face_meets_the_gate`,
   `load_balancer::a_dialog_admitted_as_new_meets_the_empty_bucket`.
6. **`received` is the source address.** A receiving hop sets `received` to
   the packet's source address whenever the sent-by host differs from it
   (RFC 3261 §18.2.1), whenever the sender asked for `rport`, even when they
   are equal (RFC 3581 §4), and whenever the hop already carries a `received`:
   a value the sender wrote is never kept. It fills `rport` with the source
   port when asked. Tests:
   `via::tests::stamping_replaces_a_received_the_sender_wrote`,
   `via::tests::stamping_adds_received_when_the_source_differs_or_rport_asks`.
7. **The `;outbound` marker stays unsigned.** Under this boundary only the
   internal plane can assert it.
8. **A single-face proxy trusts every sender.** Its one face is the internal
   face, so any sender may claim worker-outbound. The runner logs a warning at
   boot. No list of trusted addresses stands in for a missing external face.
9. **Emergency stays unauthenticated.** A `Resource-Priority` of `esnet.0`,
   `wps.0` or `q735.0` arriving on the external face still bypasses the gate and
   the new-call caps. Such a request still goes to a worker, never to a host the
   sender names. The external plane is semi-trusted for this.

## Consequences

- The external face's rules assume a strategy whose cookie is authenticated.
  `ForwardAllStrategy` decodes an unsigned `target=` and so forwards a request
  to any host its sender names; it is a test strategy, never one for an
  external face.
- A worker rebooted onto a pod address the registry does not list yet sends
  its requests out through the marker (D2), but the answers to them arrive on
  the external face and are dropped (D4) until the registry lists the address:
  the registry projects ready workers only, and a new address is no departed
  one. The far end's retransmissions bring the answer again once the worker is
  listed within the transaction's lifetime (32 s: Timer F, or the 2xx
  retransmission window). A worker that
  originates requests while it stays not ready for longer loses those
  transactions.
- A sender placed on the internal plane is trusted whatever it is: a caller
  that dials the internal address bypasses every rule above.
