# 0007 — Transaction layer Rust shape: actor + single DelayQueue

**Status:** accepted (2026-05-31)

## Context

The transaction layer (RFC 3261 §17 client/server FSMs + retransmission timers)
runs on multi-threaded tokio. Its ingest path, its send API and every firing
timer touch one transaction map, and a call holds several timers per
transaction, so three choices bear on scalability: how timers are held, who
owns the map, and what the layer's scope is. The scalable option was chosen
for each.

## Decision X1 — timers: one `DelayQueue` driver, not a task per timer

A task per timer spawns ~2 tokio tasks per client txn + 1 per completed server
txn. At 50K concurrent calls that is ~100–150K timer tasks — viable but heavy
on scheduler bookkeeping and memory.

**Chosen:** a single [`tokio_util::time::DelayQueue`] holds every pending SIP
timer, keyed by role and branch. One driver pops due timers. Memory is flat in
the number of *pending* timers, not tasks, and there is a single wakeup path.

This does **not** contradict the sip-clock crate's caveat that wrapping
scheduling "would re-implement a worse tokio" (`crates/sip-clock/src/lib.rs`):
`DelayQueue` *is* tokio's timer wheel (it rides
`tokio::time`), not a hand-rolled one. `tokio::time::pause`/`advance` therefore
drives it in tests exactly like every other behavioural timer.

Retransmit progression (`interval`/`elapsed`, INVITE-doubles vs.
non-INVITE-caps-at-T2) is carried on the transaction and re-inserted on each
fire, giving the §17.1 send cadence (sends at +500 / +1500 / +3500 / … ms).

## Decision X2 — the txn map: an actor owns it, no shared lock

On tokio the ingest path, the send API, and every firing timer would race a
shared map.

**Chosen:** a single **owner task** ("the actor") owns the `HashMap` and the
`DelayQueue` and is the **only writer**. It `select!`s over (1) the external
send API (commands over an mpsc; callers await a oneshot reply), (2) inbound
packets it `recv`s and parses **inline**, (3) the next timer expiry, (4) the
safety-net sweep. No `Mutex`, no `DashMap`.

Rejected `Arc<Mutex<HashMap>>` (a global lock is a contention point under load
and timer tasks would block the recv path) and sharded/`DashMap` (ordering
subtleties; weakest fit for the single-writer/FIFO seam the HA plan preserves).

The metrics surface is backed by shared atomics the owner updates **before** it
replies to a command, so a synchronous read right after an `await` reflects the
mutation (e.g. `active_transactions()` is `== map.len()` immediately after
`send_request().await`).

## Decision X3 — scope: the transaction layer alone

The transaction layer is its own crate, `sip-txn`, without the per-call
dispatch above it: **the transaction layer is shared by the proxy and the
B2BUA, whereas the per-call FIFO is a B2BUA-only concern** (ADR-0010).
Coupling the FSMs to the dispatcher would drag a B2BUA concept into the
proxy's path. The single-writer property the dispatcher provides downstream
is, *at this layer*, already provided structurally by the actor (X2).

New-call admission is not this layer's either: it is the admission ladder
above it (`b2bua::admission`, ADR-0037). This layer judges only the backlog
rung (ADR-0037 item 6) and answers a copy of an INVITE the node already
refused.

## Decision X4 — transaction identity: one map per role, the §17 match rules

A request matches only a server transaction and a response only a client one,
so the actor keeps one map per role. A client transaction is keyed by its
branch; a response matches it on branch and CSeq method (§17.1.3). A server
transaction is keyed by the request's top-Via branch, sent-by and method, an
ACK naming its INVITE's transaction and a CANCEL its INVITE's (§17.2.3, §9.2).
A request that comes back to the node that sent it, with no other hop's Via
on top, therefore opens a server transaction beside its sender's client one.

- **Sent-by** compares the host case-insensitively and the port as written:
  an omitted port is not an explicit 5060, as an omitted default component
  does not match an explicit one in a URI (§19.1.4). `received`, `rport` and
  the transport are no part of it, so a hop's §18.2.1 stamping leaves a copy
  matching. The comparison lives in `sip-message` (`SentByRef`, ADR-0025).
- **Collisions.** Two senders that pick one branch are two transactions —
  §17.2.3 compares the sent-by because branch uniqueness (§8.1.1.7) holds per
  UA only. A sender that reuses a branch for another method is read the same
  way: §17.2.3 names the method among the match conditions, so the second
  request is a new transaction, not a retransmission. Each gets the whole
  §17.2 behaviour, and a CANCEL matches only the INVITE of its own sent-by.
- **Cost.** A message is matched by an identity borrowed from it: one hash
  lookup, no allocation. Opening a server transaction copies its key into the
  map, the call index and each of its timers.

## Not exposed

- **A per-(method, role, state) breakdown gauge** — the `method` field is
  carried so it is a pure addition.
- **A traced send error** — a send error is counted
  (`b2bua_txn_send_errors_total`), not traced.

## RNG seam

Tags and branches come from `IdGen` — a small **injectable value** (not a
trait), mirroring the clock seam: `IdGen::seeded(seed)` for deterministic tests,
`IdGen::from_entropy()` in production.

A response is matched to its client transaction by branch and CSeq method
(§17.1.3), so identifiers must be unguessable off-path: each is HMAC-SHA256
over a counter, keyed from the OS RNG in production (the seed is the key in
tests). The response's source is not compared with the transaction's
destination: RFC 3261 §18.1.2 does not ask it, a NAT, load balancer or
multi-homed peer answers from another address, and the on-path host left able
to forge can spoof the source too.

## No property / parity tests

The transaction layer has no property test and no parity check: no layer runs
them, and ADR-0005 says why the network layer has neither. The four
behavioural suites are the whole test surface.

## References

- [`crates/sip-txn/src/layer/`](../../crates/sip-txn/src/layer/) — the actor
- [ADR-0005 — network layer Rust shape](./0005-network-layer-rust-shape.md)
