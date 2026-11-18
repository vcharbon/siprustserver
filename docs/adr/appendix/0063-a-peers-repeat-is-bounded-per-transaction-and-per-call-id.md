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
| 2xx replayed for an INVITE copy, Accepted (RFC 6026) | `layer/server.rs` | re-sends the INVITE on every 2xx | Timer L 32 s | §1 (absorbed but on a seeded transaction) |
| Non-INVITE response replayed for a request copy | `layer/server.rs` | re-sends the request on every response | Timer J 32 s | §1 |
| 200 to a CANCEL copy on an answered INVITE | `layer/server_cancel.rs` | re-sends the CANCEL on every 200 | the INVITE's hold, up to 32 s | §1 |
| 481 to a CANCEL matching no INVITE, stateless | `layer/server_cancel.rs` → router | re-sends the CANCEL on every 481 | none | §1 (its own transaction), §3 |
| Hop ACK to a non-2xx final copy | `layer/held_ack.rs` | re-sends the final on every ACK | Timer D 32 s, after the call's removal included | §1 |
| Orphan's re-ACK of a 2xx copy, up to 16 dialogs | `layer/orphan.rs` | re-sends the 2xx on every ACK | Timer M 32 s; outside ADR-0057 §10 | §1, counted per dialog |
| Refusal replayed for a copy of a refused INVITE (backlog, ingress brake) | `layer/backlog.rs`, `b2bua/src/ingress_brake.rs` | re-sends the INVITE on every refusal | 64·T1 memo from the first refusal | §1 (the brake answers on the receive thread, from its memo) |
| Timer G, Timer A and Timer E ladders | `sip-retransmit` | none: proactive, not re-armed by copies | `MAX_RUNGS` 64 | not a loop |

## Router (`b2bua`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| 481 or 405 to a new in-dialog request for an unknown or removed call | `router/unroutable.rs` | answers every 481 with a new request | none (a 1:1 flood) | §3 |
| ACK, plus a new BYE transaction for a 2xx, to a final for a call this node does not hold or has removed | `router/unheld.rs` → `b2bua-core` `unheld_response` | re-sends the final on every ACK | none; each copy opens a new BYE | §2 (the book), §3 |
| Dialog-gate answers (481, 487, 500, 200) on an ending dialog | `router/dialog_gate.rs` | answers each with a new request | the lifetime cap's removal | §3 |
| Answers after the lifetime cap (200 to BYE, 481) | `dispatch/queue.rs`, `router/owed.rs` | answers each with a new request | the queue's release | §3 |
| 482 to a late copy of the caller's initial INVITE | `router/admit.rs` | re-sends the INVITE on every 482 | its transaction's Timer H, then judged again | §1 (the count outlives the transaction) |

## Call layer (`b2bua-core`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| Re-ACK of a 2xx copy, fork stragglers included | `rules/relay/repeat.rs` | re-sends the 2xx on every ACK | ADR-0057 §10 | §2 |
| Re-ACK of a non-2xx copy whose ACK the record kept | `rules/evidence.rs` | re-sends the final on every ACK | the per-interval cap's teardown only; the rule runs while terminating | §2 (the action) |
| ACK to an unheld non-2xx final on a held call | `rule_chain/unheld_final.rs` | re-sends the final on every ACK | the per-interval cap's teardown only | §2 (the book) |
| ACK and BYE to a 2xx naming a leg the record lacks | `rules/actions/evidence.rs` `release_unknown_leg_2xx` | re-sends the 2xx on every ACK | none; a new BYE per copy | §2 (the book) |
| Local refusals (491, 500, 513, 403, 405) and relays of in-dialog requests | `rules/actions/respond.rs`, `rules/actions/relay_request.rs` | answers each final with a new request | the per-interval cap's teardown | §3 |
| One-shot ACKs (crossing 2xx, ended-dialog 2xx, machine phases), the PRACK, the relayed ACK, keepalive OPTIONS, takeover seeds | various | none: once per event or phase | one-shot | not a loop |

## Proxy (`sip-proxy`)

| Emission | Site | Closing peer | Bound before | Bound by |
|---|---|---|---|---|
| Stateless forwarding of every message | `core/request`, `core/response` | any loop between a peer and the B2BUA | none per call | §3 at the B2BUA end |
| Refusal replayed for a copy (503, 513, 200 to the CANCEL) | `core/request/reply.rs` | re-sends on every refusal | 64·T1 memo | §1 |
| Preflight rejects (483, 482, 420) | `core/request/route.rs` → `reply.rs` | re-sends the same request on every reject | none | §1 (the refusal memo) |

A peer that answers each proxy refusal with a fresh request never reaches the B2BUA, so no
budget sees it. It is a 1:1 flood, bounded by the proxy's self-gate and load shedding, and
ADR-0063 accepts it.

## The RFC reading (§1)

RFC 3261 asks for an answer to every copy in four places:

- an INVITE server transaction in Proceeding re-sends its provisional (§17.2.1, MUST);
- one in Completed re-sends its final (§17.2.1, SHOULD);
- a non-INVITE server transaction re-sends its last response (§17.2.2, MUST);
- an INVITE client transaction in Completed re-sends its ACK on every final retransmission
  (§17.1.1.3, MUST).

A retransmission is a copy sent on the T1→T2 schedule of §17.1.1.2 and §17.1.2.2. Its first
re-send comes T1 after the original, the gaps never shrink, and the client stops at Timer B or
F, 64·T1 after the original. ADR-0063 reads two kinds of copy as no retransmission, as ADR-0057
§10 reads §13.2.2.4 for a 2xx:

- a copy past the class's re-sends that arrives less than T1 after the last answered one;
- a copy later than 64·T1 after the first.

Every copy on the schedule is answered, a lossy peer's included. A peer with a larger T1 is
answered until our 64·T1 window closes.

RFC 6026 §7.1 has an Accepted INVITE server transaction absorb INVITE copies. Its 2xx is re-sent
by the TU on its own schedule. A seeded transaction whose 2xx has not yet left answers the copy
with it, because after a takeover the copy is the first chance to send it.

## Sizing the budget (§3)

The lifetime cap counts every request and every response the dispatcher is offered: each
provisional, each final and each forked 2xx is one counted event. For each counted event, a
compliant peer and the node put at most 1 + R of each of these kinds of message on its
Call-ID, where R is the largest `Class::resends()` (10, the non-INVITE ladder capped at T2):

- the message and its copies;
- the node's answers to those copies (a replayed provisional or final);
- the node's own re-sends (Timer G);
- the hop ACKs to a final.

That is 44 messages. A call hairpinned through the node sees each message twice, once sent and
once received, which gives 88. A transaction that carries no counted event ends its call (an
unanswered keepalive). A budget of 100 × `max_messages_per_call_lifetime` is therefore out of
reach of a call the lifetime cap has not yet ended. The default, 10 000 000, meets that
relation at the lifetime cap's default of 100 000, which ADR-0057 §9 sizes.

## Detection (§6)

- `b2bua_txn_message_budget_trips_total` counts trips, and the trip logs its Call-ID.
- `b2bua_cap_trips_total{cap="messages_per_call_id", kind="peer"}` counts the calls ended. A
  peer cap is a counter, not an alert (ADR-0057 §7).
- `b2bua_txn_copies_past_schedule_total{kind}` counts what §1 absorbs: `request`, `cancel`,
  `non-2xx`, `2xx` (an orphan's), `refused-invite` (the shared memo's, the ingress brake's
  included). The proxy counts its own on `sip_proxy_rejects_total{reason="copy_past_schedule"}`.
- `b2bua_copies_past_schedule_total{kind}` counts what §2 absorbs: a call's re-ACK of a 2xx or
  a non-2xx, and the book's.
- `b2bua_txn_muted_messages_total{direction}` counts what a mute drops.

The transaction layer's families carry the `b2bua_txn_` prefix the worker serves them under.
