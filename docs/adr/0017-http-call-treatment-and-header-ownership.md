# 0017 — HTTP call-treatment model, header ownership, and the reroute plan

**Status:** accepted (2026-06-07)

## Context

The B2BUA decides *how to treat a call* through an external decision layer (a
future HTTP adapter; today the in-process `ScriptedDecisionEngine`). This is the
substrate for **advanced numbering-plan services**: an inbound INVITE is
described to the layer (R-URI, From/To, all non-structural `X-*` headers, body),
and the layer answers with an instruction the B2BUA executes.

We want a single, expressive instruction vocabulary so "all the info of how to
treat the call" travels as one decision, covering:

1. **Route + rewrite** — bridge the call and update identity fields (From number,
   To number, R-URI) and add arbitrary headers (PAI, PANI, any `X-*`).
2. **Route and reroute** — an *ordered list* of destinations; on a b-leg failure
   the next is tried, and when the list is exhausted the caller is rejected
   *transparently*, the way the plan dictates.
3. **Direct rejection / redirect** — author a final failure (with a `Reason`
   header, RFC 3326) or emit a **302** carrying a **Contact list** to redirect
   the caller.

The pre-existing shapes (`schemas.rs`) covered only part of this and, crucially,
several declared fields were **unwired**: `RouteDecision.update_headers` was
never applied in `apply_route`, and `reject_call` took only `(code, reason)` —
ignoring `RejectDecision.update_headers`. Failover was a single fixed path
(`CallFailureResponse::{Failover, Terminate}`, `Terminate` → hardcoded 486).

A second-order constraint surfaced during design: the b-leg INVITE generator
*owns* the structural headers (From/To/Via/CSeq/Call-ID/Contact/Max-Forwards)
from typed opts; the flat header-update map only **appends** non-structural
headers. So "rewrite the From/To number" is not expressible as a map entry — a
flat `update_headers["From"]` appends a *second* From header rather than
rewriting it.

## Decision

### X1 — One unified `CallTreatment` returned at every hop

Both `new_call` and `call_failure` return the **same** closed set, replacing the
old per-callback enums (and the fixed `Terminate`):

```
enum CallTreatment {
    Route(RouteDecision),       // bridge to a destination + identity/header rewrites
    Redirect(RedirectDecision), // 302 to the caller with a Contact list
    Reject(RejectDecision),     // layer-authored final failure (code, reason, headers)
    Relay,                      // pass the last attempted b-leg's failure verbatim
}
```

`new_call` may return `Route | Redirect | Reject` (no prior leg, so `Relay` is
not meaningful). `call_failure` may return any of the four. This makes the
numbering plan able to do *anything at any hop*: try the next destination,
redirect, author a rejection, or relay the real downstream failure.

### X2 — Header ownership matrix

For each header on a B2BUA-authored message, exactly one party is its author —
the **decision layer** or the **core engine**. For a normal service:

| Header | Owner | Notes |
|---|---|---|
| From / To **URI** | decision layer | via typed `new_from` / `new_to` |
| From / To **tag** | core engine | never HTTP-settable |
| R-URI | decision layer | existing `new_ruri` (full R-URI) |
| Contact | core engine, **except** decision layer on a **302** | redirect targets |
| Via, CSeq, Call-ID, branch, Max-Forwards | core engine | always |
| PAI, PANI, any `X-*` | decision layer (additive) | flat `update_headers` |

Structural rewrites (From/To URI) go through **typed identity fields**
(`new_from`, `new_to`), never the flat map; the map only appends non-structural
headers and removes non-structural headers. Tags are never HTTP-settable.

### X3 — The reroute plan rides the opaque callback context (callback-per-failure)

Failover stays **callback-per-failure** — the architecture a real HTTP backend
needs. The platform treats `callback_context` as an opaque token it round-trips
untouched. The decision layer stashes the **reroute plan** there — the ordered
remainder of destinations plus the `on_exhausted` treatment. On a b-leg failure,
`call_failure` pops the head → returns a `Route` to it, and re-stashes the tail
(`apply_route` already persists the returned route's `callback_context` back onto
the call). On an empty list, it returns the plan's `on_exhausted` treatment
(`Reject` / `Relay` / `Redirect`) — the per-call "reject transparently" choice.

In tests, Alice injects the entire plan via the `X-Api-Call` header; the scripted
adapter is stateless and simply walks the plan it reads back out of the context.

### X4 — HA: zero new replicated fields

Because the plan rides the **existing** `Call.callback_context: Option<String>`
(line 468 of `call/src/model.rs`, a plain serialized field) and `Call.ext`, it is
msgpack-encoded and replicated with the call **for free**. No new field is added
to `Call`, so there is **no positional-codec change** (ADR-0008) and no HA
migration. The plan, and thus the in-flight numbering-plan state, survives
failover **by construction** — both reactive takeover and reclaim deserialize the
full body including the context.

### X5 — Edge-case defaults (overridable)

- **`Relay` with no captured downstream failure** (e.g. relay requested before
  any b-leg produced a final response): fall back to a synthesized `480
  Temporarily Unavailable`. Relay is only verbatim when a real failure exists.
- **Multiple `Reason` headers** (RFC 3326 allows SIP + Q.850 rows): the flat
  `update_headers` map (`BTreeMap<String, Option<String>>`) carries **one** value
  per name, so only a single `Reason` is expressible today. The response
  generator's `extra_headers: Vec` already permits duplicates, so a future typed
  `reason: Vec<...>` field can lift this without a wire change. Single `Reason`
  suffices for the current scope.
- **302 Contact ordering**: `contacts` is an ordered `Vec<{uri, q}>`; rendered in
  list order with `;q=` params, no platform reordering (the q-value is advisory
  for the caller).
- **Redirect mid-reroute**: allowed — a `call_failure` hop may return `Redirect`,
  abandoning the hunt and 302-ing the caller.

## Considered alternatives

- **Embed the route list as a structured top-level field on the decision and
  walk it locally in the b2bua** (no per-failure callback). Rejected: it would be
  a *new replicated field* on `Call` (codec change), and it diverges from the
  real-world HTTP model where the backend holds plan state and receives an opaque
  token. The callback-per-failure model keeps the platform agnostic and the wire
  format frozen. (The opaque-token form gives the same "one big JSON" ergonomics
  in tests, since the token *is* the plan there.)
- **Magic `From`/`To` keys in the flat header map** routed into generator opts.
  Rejected: relies on special-cased keys, collides with the "removals never apply
  to structural headers" rule, and hides identity rewrites inside a map meant for
  additive headers.
- **Decision owns the full header set.** Rejected: breaks the Via/CSeq/Call-ID/
  branch/tag invariants the generator guarantees — unacceptable blast radius.
- **Keep separate new-call vs failover enums.** Rejected in favor of one
  `CallTreatment` — less duplication, and the plan's `on_exhausted` is literally a
  treatment, so the sets are identical anyway.

## Consequences

- The decision-engine trait contract changes (unified return type) — a one-time
  churn across the scripted adapter, `apply_route`, and `initial_invite`.
- `apply_route` must thread `route.update_headers` into `build_b_leg`'s existing
  `header_updates` slot, and `new_from`/`new_to` into the generator opts.
- `reject_call` must thread `update_headers` into the existing `extra_headers`
  slot and gain a Contact-list path for 302 (`response_to_a_leg` already has both
  `contact` and `extra_headers` parameters).
- No HA/codec migration (X4). Failover transparency (ADR-0014) is preserved: the
  plan is already-replicated state, not a runtime dependency on the decision
  service being reachable during takeover/reclaim.
