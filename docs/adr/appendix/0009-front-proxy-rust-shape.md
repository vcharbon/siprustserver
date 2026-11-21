# Appendix: Front proxy + load balancer Rust shape

The decision part is [ADR-0009](../0009-front-proxy-rust-shape.md).

## Decision X1. Stateless proxy: a branch computed from the message, no transaction FSMs

`ProxyCore` is stateless (RFC 3261 §16): it manipulates Via / Record-Route /
Route directly and forwards, never instantiating a transaction state machine on
its data path, and `sip-clock::Clock` gives its timestamps. CANCEL/ACK correlation is a proxy-local LRU (`cancel_lru`) keyed
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

**The branch is a function of the message.** The pushed branch is a SHA-256 over the received
top Via's branch and sent-by, the Call-ID, the From tag and the CSeq number (`crate::branch`).
A retransmission, its CANCEL and its non-2xx ACK compute the same branch at any instance, so the
downstream server transaction (§17.2.3) matches them to the first forward whether or not a memo
entry survived. The proxy receives only RFC 3261 branches (the parser refuses a top Via without
the magic cookie), so the branch input set is §16.11's first. Rejected: a random
branch kept in the memo. Two active instances share no memo (X7), and an evicted entry would split
one transaction across two branches.

**Two Record-Route entries.** The proxy Record-Routes an initial dialog-creating request with
two entries of its own: a cookie entry, which the external party's in-dialog requests follow to
the worker (decoded to `w_pri` and looked up in the registry, so it survives a worker's address
change), and an `;outbound` entry, which the worker's requests follow toward the external party.
In-dialog direction is read off the proxy's own Record-Route, never off a marker the worker
stamps: the §12.1.1/§12.1.2 route-set rules put the right entry on top for each party, and the
proxy only orders the pair for the next hop. For a worker-originated request the cookie names
the worker by its top Via sent-by, not the UDP source: behind a masquerading node the source is
the node address and an ephemeral port that names no worker.

## Decision X3. Why rendezvous placement

HRW is chosen over a consistent-hash ring: an O(N) scan over a pool of a few workers, no virtual
nodes to tune, and a membership change moves only the keys of the worker that joined or left
(1/N expected churn). The winner is a pure function of the Call-ID and the alive set, so every
instance, and a restarted one, places a call on the same worker with no shared state (X7).

## Decision X4. The self-gate's shape

The gate sheds only external, new-dialog, non-emergency INVITEs. A worker's own INVITE always
bypasses it: refusing a worker's outbound leg causes rerouting churn, not less load. It reads
intake pressure as packet age at dequeue, full pressure at T1 (500 ms, when callers' Timer A
retransmissions start compounding the overload), not runtime utilisation: a saturated serial
recv shard reads busy while the runtime's other threads idle. It is binary (a threshold and a
hard CPS bucket), not AIMD like the per-worker caps, because the proxy's load is its own, with no
second party to converge with. Its token bucket refills on a `tokio::time::Instant` timeline.

## Decision X6. The overload observer

- **Explicit time.** `WorkerLoadObserver` takes the current time as a `now_ms` argument on every
  call and arms no timer, so its ladder is a pure state machine tests drive with literal
  timestamps. The production caller passes `Clock::now_ms()`, which advances with a paused
  tokio runtime, so no second clock needs syncing.
- **A stale payload floors the cap.** A worker whose `X-Overload` payload stops arriving while
  it stays Alive has its cap decreased once per sweep after `payload_stale_ms`. Keeping its last
  cap was rejected: it admits at full rate into a worker the proxy has no telemetry on. The stale
  window sits well above the probe cycle (interval plus reply window), so a worker that answers
  is never floored, and each floor is counted.
- **Aggressive defaults.** The shipped bands and AIMD factors (soft 0.4, hard 0.5, critical 0.65,
  decrease 0.5) take a loaded worker out of non-emergency new-dialog selection early, so it keeps
  CPU for emergency and established calls: a worker that saturates before it sheds degrades the
  calls it holds. They are calibration starting points, not measured values; every band,
  hysteresis, AIMD and cap field is an environment setting.

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

**A non-2xx ACK is told by the ACK-hop memo.** The ACK-hop memo, written for every relayed
non-2xx INVITE final and keyed with the received branch, tells a non-2xx ACK from a 2xx ACK,
not the INVITE's own entry and not a Route header. A worker's outbound INVITE carries a
preloaded Route and §17.1.1.3 makes its non-2xx ACK copy it, so a Route test would route exactly
that ACK as a 2xx ACK. A takeover worker or a restarted UA whose reset branch generator repeats
the INVITE's branch sends a 2xx ACK matching that INVITE's entry; no non-2xx final is relayed for
a confirmed call, so it finds no ACK hop and routes in-dialog. The proxy never sends the
hop-by-hop ACK itself (ADR-0022 X4): the caller's ACK is the only one, and a lost ACK on the
caller's hop recovers by the final's retransmission.

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
