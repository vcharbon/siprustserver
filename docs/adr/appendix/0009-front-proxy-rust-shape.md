# Appendix: Front proxy + load balancer Rust shape

The decision part is [ADR-0009](../0009-front-proxy-rust-shape.md).

## Decision X1. Stateless proxy: reuse `IdGen`, not the transaction FSMs

`ProxyCore` is stateless (RFC 3261 §16): it manipulates Via / Record-Route /
Route directly and forwards, never instantiating a transaction state machine.
So `sip-proxy` does **not** depend on `sip-txn`'s FSMs; it reuses only
`sip-txn::IdGen` (the RNG seam) for Via branch generation, and `sip-clock::Clock`
for timestamps. CANCEL/ACK correlation is a proxy-local LRU (`cancel_lru`) keyed
on the INVITE transaction as received, its top-Via branch and sent-by (RFC 3261
§9.1, §17.2.3), so it works at any hop, keeps a spiral's two passes apart, and
survives the load balancer re-sharding a fallback selection. A CANCEL matching no
remembered INVITE transaction is forwarded statelessly (§16.10) as X7 places it,
where a worker holding no such INVITE answers it 481 (§9.2).
Reverse-path failover of an INVITE response to a dead worker reads the backup
from the bottommost of this proxy's own Record-Route entries whose cookie names
that worker as primary: a spiral puts one pair per pass on the response, and the
pair recorded relaying the worker's request out lies below every later pass (a
later pass reaching the same worker may name another backup, the alive set having
changed). A response that carries none of this proxy's Record-Route entries is
read on the Via this proxy pushed instead. That is the answer to an in-dialog
request: this proxy never Record-Routes one (`core/request/record_route`), and an
answer carries the Record-Route only of a dialog-creating request (RFC 3261
§12.1.1). On a worker's in-dialog INVITE, and on its CANCEL and ACK, that Via
carries the cookie of the request's own route set, the strategy's own fields
alone, once they verify for the call (`RoutingStrategy::verified_cookie`,
`core/request/via_cookie`), never over a param the proxy set itself
(`headers::via_with_cookie`). Read off the route set, it is the same on the
INVITE, its CANCEL (§9.1) and its ACK (§17.1.1.3) at any proxy instance. A
worker's initial INVITE states no cookie of its own, so its Via carries none;
its dialog-creating responses bring the cookie back on the Record-Route.
An in-dialog request whose Route set holds several of this proxy's pairs back to
back (a spiral with no Record-Routing hop between passes) takes its direction
from the first entry of the last pair: it leaves this proxy on the last pass, and
its worker Via does not make it worker-outbound when that pair decodes. An odd run
of own entries is no run of pairs this proxy recorded: its first entry decides,
and a worker Via still makes the request worker-outbound. These direction rules
hold on the internal face only (ADR-0048).


## Decision X7. Two active instances: the memo, and every path without it

Two proxy instances run active behind one advertised address. An ECMP hop hashes each
flow on its own, so the requests and responses of one transaction cross different
instances in steady state (the caller's flow on one, the worker's on the other), and a
flow moves to the other instance when a next hop is added or removed.

**Stateless by default, a memo for the exceptions.** A call placed on its rendezvous
winner routes from either instance with no memo. The spill and the band filter are a
safety valve: the deployment's sizing sets the AIMD ceilings so neither acts at nominal
load. A placement off the winner is therefore exceptional (overload, a draining
winner, a membership change), and the memo covers it at the instance that placed it.

**The memo.** Each instance keeps its full branch memo (`cancel_lru`): an entry for every
INVITE it forwards, a retransmission entry for every request but a CANCEL, and an ACK
hop for every non-2xx INVITE final it relays, whether or not the INVITE crossed it. An
INVITE's entry is shortened to Timer H (the non-2xx ACK window, RFC 3261 §17.2.1) when
its final passes the instance (`core/response/final_memo`); the workers' INVITE
transaction bound plus Timer H (`PROXY_INVITE_TXN_TIMEOUT_SEC`, one chart value with the
workers' `B2BUA_INVITE_TXN_TIMEOUT_SEC`) stays the cap of an entry whose final crossed
the other one. The worker's flow and the caller's hash on their own, so in steady state
a call's legs often cross: at one instance a call holds, at most, one leg's INVITE entry
to its cap (the leg whose final crosses the other instance) and its other leg's entry for
the ring, then Timer H; every further INVITE (a re-INVITE, a rerouted or transferred leg)
may cross to its cap too. The memo is sized on that working set at the deployment's call
rate, mean ring, INVITEs per call and cap (`cancel_lru::entry_ms_per_call`), so its
eviction happens only under abnormal load. The instance that placed a call knows where it went, a placement
away from the call's rendezvous winner included (a spill past an empty bucket, a
band-filtered winner, a winner refusing new calls, a winner away or not yet back when
the call came).

**Without the memo** (the other instance after a flow move, or an entry evicted under
abnormal load) every request of an admitted transaction still routes, and none meets a
gate:

- A worker's CANCEL and non-2xx ACK repeat their INVITE's Route and Request-URI
  (§9.1, §17.1.1.3) and go where it went: the loose Route past the proxy's own, else the
  Request-URI.
- A request carrying the dialog's cookie goes where the cookie places it, the CANCEL of
  a re-INVITE included.
- A caller's CANCEL or non-2xx ACK goes to the rendezvous winner for its Call-ID among
  the alive workers (`RoutingStrategy::placement_by_rank`): no band filter, no AIMD
  token, no admission gate, never a 503. With no alive worker it is dropped and counted
  (`sip_proxy_no_target_dropped_total`, `kind="cancel"` or `"initial"`). An ACK reaching
  the instance that placed the INVITE but did not relay its final follows the INVITE's
  entry.
- Responses follow their Via, each instance applying the owner rule (ADR-0055 §8) on
  its own view. The ACK of a final an instance answered itself is recognised by its
  To-tag, derived under the shared key (ADR-0046 §7), at either instance.

**Accepted residuals.** Each has a cell in `sip-proxy`, or an hakit incident that
measures it:

- *A call placed away from its rank winner, reached without its entry.* A call placed
  away from its rendezvous winner among the alive workers (a spill, a band filter, a
  draining winner, a winner rejoining or a worker joining while the call rings) whose
  CANCEL reaches an instance without its entry (the caller's flow moved, or the entry
  was evicted) goes to the winner, which holds no such INVITE and answers 481 (§9.2);
  the call rings on at the worker holding it until its own final. Its non-2xx ACK, when
  the final crossed an instance other than the one the ACK reaches, reaches the winner
  too, which discards it, and the holder stops retransmitting at Timer H. Cells:
  `active_active::off_rank`, `active_active::residual`.
- *An INVITE copy after a flow move.* A UDP INVITE is retransmitted from T1 until its
  first provisional reaches the caller (about T1 to 2·T1 after it, longer when the 100
  is lost). A copy reaching the instance that never saw the first is a new call there:
  no message field tells it apart, and the instances share no state. It meets that
  instance's gate and caps: admitted, it lands on the rendezvous winner under the
  INVITE's own §16.11 branch, where the worker's server transaction absorbs it
  (§17.2.3); with that winner's bucket empty it spills to another worker and opens a
  second call, and with every bucket empty, or the gate shut, it draws a 503 for a
  call already ringing. A re-INVITE carries a To-tag and its dialog's cookie and meets
  neither. Cells: `active_active::caller_side`, `active_active::residual`.

- *A proxy restarted in place.* A proxy that crashes and restarts on its node inside the
  route guard's dead bound plus the routers' BFD detection keeps the node in the routers'
  ECMP (ADR-0056 §10): its flows come back to an empty memo, the paths above with no
  entry. The hakit incident `kill_proxy` accepts a restart inside that bound.
- *A client that varies its source port.* An upstream client sending one call's requests
  from several source ports or addresses spreads them over flows the routers hash apart:
  in steady state its CANCEL can reach the instance that never saw the INVITE and goes to
  the rank winner, right for a call placed there, the first residual above for a call
  placed away from it, with no flow move. Cells: `active_active::off_rank`,
  `active_active::residual`.

**A CANCEL or ACK matching no call** costs one forward to a worker and that worker's 481
(an ACK draws nothing), never more: no state is written for it and nothing is
amplified. It counts in `sip_proxy_cancel_lookups_total{outcome="miss"}`, which with a
full memo holds little else: the CANCELs of calls whose INVITE crossed the other
instance.

**Two health views of one primary.** Each instance reads a worker's health from its own
probe. While one reads a call's primary dead and the other alive, what crosses the first
goes to the call's backup and what crosses the second to the primary: the case of one
proxy reading a live primary dead, where the backup's copy relays and stands down on the
primary's write (ADR-0047 D7) and a dead notice from either instance is advisory, the
backup confirming by itself (ADR-0047 D1). The window lasts until both instances read
the worker alike: one probe verdict (`HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS` +
`HEALTH_TIMEOUT_MS`, 3.6 s by default, the 1.6 s window holding the three Timer E copies
of a probe; ADR-0047 D1) when one instance lags the other, the partition's length when only
one instance has lost its path to the worker. Cell: `active_active::health_views`.

**Trace spans are per instance.** A span armed to close on the ACK of a rejecting final
(`arm_close_on_ack`) at the instance that relayed the final stays open to its idle TTL
when the ACK crosses the other instance. Observability only.

Every crossing has a cell in `sip-proxy` `tests/it/active_active`: two instances
advertising one address over one simulated fabric, each party's flow assigned to an
instance, each request sent to the instance whose memo holds nothing of it.
