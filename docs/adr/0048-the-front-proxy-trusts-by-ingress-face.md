# The front proxy trusts by ingress face

**Status:** accepted (2026-10-12)

**Owner:** `crates/sip-proxy/src/core/request/classify.rs` (a request's
direction), `core/request/admission.rs` (the gate and the To-tag rule),
`core/response.rs` (the response rule), `crates/sip-message/src/header/via.rs`
(`received`), `crates/sip-proxy-runner/src/config.rs` (the single-face warning).

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
   plane and is trusted. The external face is not.
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
   Its second Via's response address (`received`/`rport` over the sent-by, RFC
   3261 §18.2.2) must be a registry worker, a departed one included while its
   tombstone lasts. Any other response is dropped and counted
   (`response_not_to_worker`). A response arriving on the internal face follows
   its Via. Test:
   `ingress_trust::a_response_on_the_external_face_whose_next_via_is_no_worker_is_dropped`.
5. **An INVITE with a To-tag that no verified cookie backs is a new call on the
   external face.** It meets the gate, and the strategy admits it under every
   new-call cap (`SelectOpts::admit_as_new`). It is never refused for its
   To-tag alone. Test:
   `ingress_trust::a_forged_to_tag_invite_on_the_external_face_meets_the_gate`.
6. **`received` is the source address.** A receiving hop sets `received` to
   the packet's source address whenever the sent-by host differs or the hop
   already carries one, and fills `rport` when asked (RFC 3261 §18.2.1, RFC
   3581 §4). A `received` the sender wrote is never kept. Test:
   `via::tests::stamping_replaces_a_received_the_sender_wrote`.
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

- A sender placed on the internal plane is trusted whatever it is: a caller
  that dials the internal address bypasses every rule above.
- On the external face, a worker's response address must be its registry
  address. A deployment that translates a worker's source address on the way to
  the proxy loses the answers to that worker's requests.
