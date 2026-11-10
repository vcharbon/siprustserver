# The front proxy trusts by ingress face

**Status:** accepted

**Owner:** `crates/sip-proxy/src/core/request/classify.rs` (a request's direction),
`core/request/admission.rs` (the To-tag rule and the gate), `core/response.rs` (the response
rule), `crates/sip-proxy/src/face.rs` (the egress face), `crates/sip-message/src/header/via.rs`
(`received`), `crates/sip-proxy-runner/src/config.rs` (the single-face warning, the wildcard
refusal), `deploy/helm/siprust/templates/masq-exempt.yaml` and `files/masq-exempt.sh` (the edge
node's drop rule).

The [appendix](appendix/0048-the-front-proxy-trusts-by-ingress-face.md) holds the gaps behind the
context, the long form of each rule with its tests, the egress table, and the full consequences.

## Context

An `;outbound` marker, a top Via sent-by and a source address are all claims a sender makes, and
none is authenticated. The recv loop knows which face each datagram arrived on, and the internal
face is reachable from the worker plane alone.
Detail: [appendix, Context](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#context).

## Decision

### 1. The ingress face is the trust

The recv loop hands the ingress face to request and response handling. The internal face is the
worker plane and is trusted. The external face is not. A dual-face proxy refuses a wildcard bind.
The routers never carry the internal VIP onto the external plane. A request reaching the
internal socket from the proxy's own external VIP is answered 482 Loop Detected (RFC 3261 §16.3).
Detail: [appendix §1](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#1-the-ingress-face-is-the-trust).

### 2. On the internal face a request is worker-outbound

On the internal face a request is worker-outbound when its direction entry carries `;outbound`,
when its source is a registry worker, or when its top Via sent-by is one. A loose Route past the
proxy's own is followed.
Detail: [appendix §2](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#2-on-the-internal-face-a-request-is-worker-outbound).

### 3. On the external face no claim of the internal plane holds

A request there is never worker-outbound. It is routed as an inbound request, by its cookie or by
a fresh selection, and a new call meets the gate. Every Route entry past the proxy's own is
removed. Each claim is counted in `sip_proxy_external_untrusted_total{kind}`, and none is refused.
Detail: [appendix §3](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#3-on-the-external-face-no-claim-of-the-internal-plane-holds).

### 4. A response arriving on the external face is relayed only to a worker

It goes to its second Via's response address, or its sent-by, when either is a registry worker.
Any other response is dropped and counted (`response_not_to_worker`). A response arriving on the
internal face follows its Via.
Detail: [appendix §4](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#4-a-response-arriving-on-the-external-face-is-relayed-only-to-a-worker).

### 5. An INVITE with a To-tag that no verified cookie backs is a new call on the external face

It meets the gate, and the strategy admits it under every new-call cap
(`SelectOpts::admit_as_new`). It is never refused for its To-tag alone.
Detail: [appendix §5](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#5-an-invite-with-a-to-tag-that-no-verified-cookie-backs-is-a-new-call-on-the-external-face).

### 6. `received` is the source address

A receiving hop sets `received` to the packet's source address whenever the sent-by host differs
(RFC 3261 §18.2.1), whenever the sender asked for `rport` (RFC 3581 §4), and whenever the hop
already carries a `received`. It fills `rport` with the source port when asked.
Detail: [appendix §6](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#6-received-is-the-source-address).

### 7. The `;outbound` marker stays unsigned

Under this boundary only the internal plane can assert it.

### 8. A single-face proxy trusts every sender

Its one face is the internal face, so any sender may claim worker-outbound. The runner logs a
warning at boot. No list of trusted addresses stands in for a missing external face.

### 9. Emergency stays unauthenticated

A `Resource-Priority` of `esnet.0`, `wps.0` or `q735.0` arriving on the external face still
bypasses the gate and the new-call caps. Such a request still goes to a worker, never to a host
the sender names. The external plane is semi-trusted for this.

### 10. A dual-face proxy crosses, with one exception the registry decides

One pure function (`face::EgressPolicy`) picks the egress face from the ingress face, the message
and the registry, with no address plan and nothing stored. A request crosses to the other face,
except one arriving internal for a registry worker or re-entering on a verified cookie, which
stays internal and carries `;same-face` on the Via the proxy pushes. Via states the egress face,
and Record-Route states both. Everything dialled through the proxy is on the external plane.
Detail: [appendix §10](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#10-a-dual-face-proxy-crosses-with-one-exception-the-registry-decides).

## Consequences

- The external face's rules assume a strategy whose cookie is authenticated. `ForwardAllStrategy`
  is a test strategy, never one for an external face.
- A worker rebooted onto a pod address the registry does not list yet loses the answers to its
  requests until the registry lists it.
- A sender placed on the internal plane is trusted whatever it is.
- A peer's Contact or Record-Route can aim a worker's request onto the internal plane only with a
  cookie this proxy signed for that Call-ID, and the worker it names answers 481.
- A worker's first hop is the front proxy whatever a callee's Record-Route says.

Detail: [appendix, Consequences](appendix/0048-the-front-proxy-trusts-by-ingress-face.md#consequences).
