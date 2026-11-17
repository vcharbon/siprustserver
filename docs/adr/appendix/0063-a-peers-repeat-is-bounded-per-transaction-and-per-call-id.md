# ADR-0063 appendix: the inventory of reactive emissions

This is the review behind [ADR-0063](../0063-a-peers-repeat-is-bounded-per-transaction-and-per-call-id.md).
Each row is an emission the node sends because a peer's message arrived. "Closing peer" names
the peer behaviour that turns the emission into a loop. "Bound before" is what stopped the loop
before ADR-0063. "Bound by" names the ADR-0063 section that bounds it.

## Transaction layer (`sip-txn`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| Last provisional replayed for an INVITE copy (a seeded transaction's first 100 included) | `layer/server.rs` `replay_cached` | re-sends the INVITE on every 1xx | the ringing time, up to 193 s | §1 |
| Non-2xx final replayed for an INVITE copy, Completed and Confirmed | `layer/server.rs` | re-sends the INVITE on every final, never ACKs | Timer H 32 s, then Timer I 5 s | §1 |
| 2xx replayed for an INVITE copy, Accepted (RFC 6026) | `layer/server.rs` | re-sends the INVITE on every 2xx | Timer L 32 s | §1 |
| Non-INVITE response replayed for a request copy | `layer/server.rs` | re-sends the request on every response | Timer J 32 s | §1 |
| 200 to a CANCEL copy on an answered INVITE | `layer/server_cancel.rs` | re-sends the CANCEL on every 200 | the INVITE's hold, up to 32 s | §1 |
| 481 to a CANCEL matching no INVITE, stateless | `layer/server_cancel.rs` → router | re-sends the CANCEL on every 481 | none | §1 (own transaction), §3 |
| Hop ACK to a non-2xx final copy | `layer/held_ack.rs` | re-sends the final on every ACK | Timer D 32 s, after the call's removal included | §1 |
| Orphan's re-ACK of a 2xx copy | `layer/orphan.rs` | re-sends the 2xx on every ACK | Timer M 32 s; outside ADR-0057 §10 | §1 |
| Refusal replayed for a copy of a refused INVITE (backlog, ingress brake) | `layer/backlog.rs`, `b2bua/src/ingress_brake.rs` | re-sends the INVITE on every refusal | 64·T1 memo from the first refusal | §1, §3 |
| Timer G, Timer A and Timer E ladders | `sip-retransmit` | none: proactive, not re-armed by copies | `MAX_RUNGS` 64 | not a loop |

## Router (`b2bua`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| 481 or 405 to a new in-dialog request for an unknown or removed call | `router/unroutable.rs` | answers every 481 with a new request | none (a 1:1 flood) | §3 |
| ACK, plus a new BYE transaction for a 2xx, to a final for a call this node does not hold | `router/unheld.rs` → `b2bua-core` `unheld_response` | re-sends the final on every ACK | none; each copy opens a new BYE | §2 (one BYE per dialog), §3 |
| Dialog-gate answers (481, 487, 500, 200) on an ending dialog | `router/dialog_gate.rs` | answers each with a new request | the lifetime cap's removal | §3 |
| Answers after the lifetime cap (200 to BYE, 481) | `dispatch/queue.rs`, `router/owed.rs` | answers each with a new request | the queue's release | §3 |

## Call layer (`b2bua-core`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| Re-ACK of a 2xx copy, fork stragglers included | `rules/relay/repeat.rs` | re-sends the 2xx on every ACK | ADR-0057 §10 | §2 |
| Re-ACK of a non-2xx copy whose ACK the record kept | `rules/evidence.rs`, `rule_chain/unheld_final.rs` | re-sends the final on every ACK | the per-interval cap's teardown only; the rule runs while terminating | §2 |
| ACK and BYE to a 2xx naming a leg the record lacks | `rules/actions/evidence.rs` | re-sends the 2xx on every ACK | none; a new BYE per copy | §2, §3 |
| Local refusals (491, 500, 513, 403, 405) and relays of in-dialog requests | `rules/actions/respond.rs`, `rules/actions/relay_request.rs` | answers each final with a new request | the per-interval cap's teardown | §3 |
| One-shot ACKs (crossing 2xx, ended-dialog 2xx, machine phases), the PRACK, the relayed ACK, keepalive OPTIONS, takeover seeds | various | none: once per event or phase | one-shot | not a loop |

## Proxy (`sip-proxy`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| Stateless forwarding of every message | `core/request`, `core/response` | any loop between a peer and the B2BUA | none per call | §3 at the B2BUA end |
| Refusal replayed for a copy (503, 513, 200 to the CANCEL) | `core/request/reply.rs` | re-sends on every refusal | 64·T1 memo | §1 |
| Preflight rejects (483, 482, 420) | `core/request/route.rs` | re-sends on every reject | none (a 1:1 flood) | out of scope |
