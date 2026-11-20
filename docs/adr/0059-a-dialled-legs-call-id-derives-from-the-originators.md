# A dialled leg's Call-ID derives from the originator's

**Status:** accepted (2026-11-11).

**Owner:** `crates/b2bua-core/src/rules/relay/call_id.rs` (the mint),
`crates/call/src/helpers/leg.rs` (`leg_call_id_number`, `same_leg`),
`crates/call/src/model/leg_space.rs` (`call_id_base`).

## Context

Captures and correlation tools join the legs of a B2BUA call on a Call-ID
convention: a dialled leg carries `<N>-<originator's Call-ID>`, matched by
`^\d+-(.+)$`. A random Call-ID per leg joins on nothing.

RFC 3261 §8.1.1.4 asks a new request's Call-ID to be unique over space and
time. A number unique over time per originator's Call-ID would take state no
node holds across calls and takeovers, or random digits that lose the leg's
ordinal.

## Decision

1. Every leg `build_b_leg` dials takes the Call-ID `<N>-<originator's Call-ID>`,
   `N` the leg id's ordinal (`b-n` → `n`), plus 10^9 for a leg minted from the
   other node's leg space (`c-n`). Every leg kind shares the one counter.
2. A `Call-ID` line past the first-ingress bound takes a minted
   `<leg>-<tag>@<host>` instead: the next hop would refuse it.
3. Two records of one leg id are one leg only when their dialled INVITEs drew
   the same From-tag (`same_leg`). The reclaim union of two copies holding a
   leg id minted twice keeps the base copy's leg and leaves the other's out.

## Consequences

- Within one copy of a call every dialled leg's Call-ID is distinct.
- The B2BUA sends a Call-ID it already used, deviating from §8.1.1.4, when:
  - an originator re-offers on a Call-ID an ended call used (a retry after a
    401/407/422/423, an INVITE after its own CANCEL): the new call dials `1-X`
    again;
  - a takeover node's write is lost and a later takeover mints the same `c-n`.
- Each such INVITE draws a fresh From-tag, so the dialog differs (§12) and
  §8.2.2.2 merge detection does not apply. The next hop sees two INVITE dialogs
  on one Call-ID.
- Events of an earlier call on a reused identity are told apart by the call
  incarnation stamp, not by the Call-ID.
