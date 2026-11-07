# The front proxy trusts by ingress face

**Status:** accepted (2026-10-12)

**Owner:** `crates/sip-proxy/src/core/request/classify.rs` (a request's
direction), `core/request/admission.rs` (the To-tag rule and the gate),
`core/response.rs` (the response rule), `crates/sip-proxy/src/face.rs` (the
egress face), `crates/sip-message/src/header/via.rs` (`received`), `crates/sip-proxy-runner/src/config.rs` (the single-face warning,
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
   - a proxy node holds both addresses on `lo` (ADR-0056), so a caller-plane
     host that routes the internal address through the node would reach the
     internal socket. The routers never carry the internal VIP onto the
     external plane (a requirement, `deploy/README.md`); on the kind lab the
     node also drops every packet to `PROXY_VIP` that arrives on the external
     interface (`deploy/helm/siprust/files/masq-exempt.sh`, re-asserted with
     its other rules);
   - a datagram the proxy sends from the external VIP stays off the internal
     socket: nothing leaves that address but through the external interface,
     local delivery included (ADR-0056 §11).
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
10. **A dual-face proxy crosses, with one exception the registry decides.** The
    egress face, and with it the socket and the advertise a message states, comes
    from the ingress face, the message and the registry through one pure function
    (`face::EgressPolicy`, single-face or crossing). No address plan enters: the
    proxy holds no CIDR, a name and its resolved address are one face, and any
    instance picks the same egress with nothing stored, so a flow that moves keeps
    its one sent-by (RFC 3261 §17.2.3).

    | Message | Condition | Leaves on |
    |---|---|---|
    | Request | arrived external | internal |
    | Request | arrived internal, worker-outbound or a loose hop | external, whatever the target |
    | Request | arrived internal, neither, target a registry worker | internal |
    | Request | arrived internal, any other target | external |
    | Response | arrived external | internal (D4: every destination is a worker) |
    | Response | arrived internal, the owner rule names a worker | internal |
    | Response | arrived internal, the proxy's own Via it pops carrying the marker | internal |
    | Response | arrived internal, otherwise | external |
    | Own final | any | the face its request arrived on (RFC 3581 §4) |

    Via states the egress face. In Record-Route the top entry states the egress
    face and the lower one the ingress face, so each party's later requests reach
    the face it talks to; the `;outbound` marker stays on the worker's half. The
    third row keeps a call that loops back
    through the platform on the workers' plane: its in-dialog request re-enters
    with two of this proxy's pairs back to back, the last pair's cookie verifying for
    the call, when no hop between the passes Record-Routes, and the workers admit SIP only from the internal VIP and the
    proxy nodes. A request on that row carries a `;same-face` flag on the Via the proxy pushes,
    and a response arriving internal stays internal on that flag alone, read off the
    Via it pops, which is this proxy's own and leaves with it. The flag travels with
    the message, so either instance reads it and nothing is stored. No Via below it
    decides a face: those are their senders' to write, and a phone behind a NAT whose
    private sent-by equals a worker's address is answered from the face it reached.
    This rests on one assumption of the deployment: everything dialled through the
    proxy, a media resource function included, is on the external plane. Tests:
    `core::crossing_tests`, `face::tests`,
    `ingress_trust::an_own_final_to_a_translated_worker_leaves_the_face_it_arrived_on`,
    `ingress_trust::own_finals_to_a_sender_near_the_workers_leave_the_external_face`.
    `failover-harness`'s `dual_face` runs the callflows through real workers.

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
- A peer's Contact or Record-Route chooses the Request-URI or the next hop of a
  worker's request, and that request leaves on the external face whatever it names
  (`crossing_tests::a_contact_naming_a_registry_worker_leaves_external`). The node's
  rule of ADR-0056 §11 drops it there; on a CNI that does not enforce NetworkPolicy,
  that rule is what protects the workers. Entries naming this proxy prove no recorded
  pass, only a cookie verifying for the call does
  (`crossing_tests::a_cookie_that_does_not_verify_for_the_call_proves_no_pass`). So a
  peer aims a worker's request onto the internal plane only with a cookie this proxy
  signed for that Call-ID, one it holds from an earlier dialog of its own: the request
  reaches the worker the cookie names, which holds no such dialog and answers 481.
- A worker's first hop is the front proxy whatever a callee's Record-Route says,
  the route set intact (`b2bua-core` `relay::egress`); standalone it follows its
  route set.
- The ACK of a non-2xx final a translated worker sent on a spiral goes to the node
  address the final came from, no registry worker, so it leaves external and misses;
  that worker repeats its final to Timer H, on a call that already failed
  (`crossing_tests::a_spirals_ack_to_a_translated_worker_leaves_external`).
