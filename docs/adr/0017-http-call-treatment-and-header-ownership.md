# HTTP call-treatment model, header ownership, and the reroute plan

**Status:** accepted

The [appendix](appendix/0017-http-call-treatment-and-header-ownership.md) holds why the
generator's header ownership shapes the vocabulary, the long form of each rejected alternative,
and where the treatment is wired.

## Context

The B2BUA decides how to treat a call through an external decision layer behind
`CallDecisionEngine` (an HTTP adapter, or the in-process `ScriptedDecisionEngine` in tests). This
is the substrate for numbering-plan services: an inbound INVITE is described to the layer
(R-URI, From/To, all non-structural `X-*` headers, body), and the layer answers with an
instruction the B2BUA executes. One instruction vocabulary carries how to treat the call:

1. **Route + rewrite.** Bridge the call, update identity fields (From number, To number, R-URI)
   and add arbitrary headers (PAI, PANI, any `X-*`).
2. **Route and reroute.** An ordered list of destinations. On a b-leg failure the next is tried,
   and when the list is exhausted the caller is rejected transparently, the way the plan dictates.
3. **Direct rejection / redirect.** Author a final failure (with a `Reason` header, RFC 3326) or
   emit a 302 carrying a Contact list to redirect the caller.

The b-leg generator owns the structural headers, so a From or To rewrite cannot ride the flat
header map ([appendix](appendix/0017-http-call-treatment-and-header-ownership.md#the-b-leg-generator-owns-the-structural-headers)).

## Decision

### X1. One unified `CallTreatment` returned at every hop

Both `new_call` and `call_failure` return the same closed set:

```
enum CallTreatment {
    Route(RouteDecision),       // bridge to a destination + identity/header rewrites
    Redirect(RedirectDecision), // 302 to the caller with a Contact list
    Reject(RejectDecision),     // layer-authored final failure (code, reason, headers)
    Relay,                      // pass the last attempted b-leg's failure verbatim
}
```

`new_call` may return `Route | Redirect | Reject` (with no prior leg, `Relay` has no meaning).
`call_failure` may return any of the four, so the numbering plan can try the next destination,
redirect, author a rejection, or relay the real downstream failure at any hop.

### X2. Header ownership matrix

For each header on a B2BUA-authored message, exactly one party is its author, the decision layer
or the core engine. For a normal service:

| Header | Owner | Notes |
|---|---|---|
| From / To **URI** | decision layer | via typed `new_from` / `new_to` |
| From / To **tag** | core engine | never HTTP-settable |
| R-URI | decision layer | existing `new_ruri` (full R-URI) |
| Contact | core engine, **except** decision layer on a **302** | redirect targets |
| Via, CSeq, Call-ID, branch, Max-Forwards | core engine | always |
| PAI, PANI, any `X-*` | decision layer (additive) | flat `update_headers` |

Structural rewrites (From/To URI) go through typed identity fields (`new_from`, `new_to`), never
the flat map. The map only appends and removes non-structural headers. Tags are never
HTTP-settable.

A non-structural header neither party states has a third source, at the bottom of the precedence
ladder. A final that answers a `/call/failure` consult also carries that consult's failing b-leg
final's relayable headers (`generators::relayable_headers`, response scope: credentials, per-leg
negotiation and a privacy-concealed identity stay withheld). Per name: explicit `update_headers`
(set or removal) > decision-typed fields > relayed from the failing peer's final > core-authored
structural. The decision layer never needs to echo the `sip_headers` it was shown on
`/call/failure`, since silence lets the peer's statement travel.

The image is scoped to the consult in flight, and that scope is the invariant. Every consult
restates it, empty when the failure drew no peer final (a ring deadline, a transaction timeout).
A final that answers something else (a setup deadline, a capacity refusal, a media-service
failure, a refused redirect) carries none of it, so a superseded attempt's `Warning` and
`Retry-After` never diagnose a peer that was not contacted.

No allow-list filters the image, so a decision-authored final restates the failing peer's
`Server` and its `Allow`, though the a-face responder is this stack, which states no `Server` of
its own. This reads transparency about who refused as the rule. Withholding the
responder-describing headers would change every failure final this stack mints.

### X3. The reroute plan rides the opaque callback context (callback-per-failure)

Failover is callback-per-failure, the architecture a real HTTP backend needs. The platform
round-trips `callback_context` as an opaque token. The decision layer stashes the reroute plan
there: the ordered remainder of destinations plus the `on_exhausted` treatment. On a b-leg
failure `call_failure` pops the head, returns a `Route` to it and re-stashes the tail
(`apply_route` persists the returned route's `callback_context` onto the call). On an empty list
it returns the plan's `on_exhausted` treatment (`Reject` / `Relay` / `Redirect`).

In tests, Alice injects the entire plan through the `X-Api-Call` header, and the stateless
scripted adapter walks the plan it reads back out of the context.

### X4. HA: zero new replicated fields

The plan rides `Call.callback_context: Option<String>` (`crates/call/src/model/record.rs`) and
`Call.ext`, which are serialized and replicated with the call. No field is added to `Call`, so
there is no positional-codec change (ADR-0008). The plan, and with it the in-flight
numbering-plan state, survives failover by construction: reactive takeover and reclaim both
deserialize the full body, context included.

### X5. Edge-case defaults (overridable)

- **`Relay` with no captured downstream failure** (relay requested before any b-leg produced a
  final): a synthesized `480 Temporarily Unavailable`. Relay is verbatim only when a real failure
  exists.
- **Multiple `Reason` headers** (RFC 3326 allows SIP + Q.850 rows): the flat `update_headers` map
  (`BTreeMap<String, Option<String>>`) carries one value per name, so a single `Reason` is
  expressible. The response generator's `extra_headers: Vec` permits duplicates, so a typed
  `reason: Vec<...>` field can lift this without a wire change.
- **302 Contact ordering**: `contacts` is an ordered `Vec<{uri, q}>`, rendered in list order with
  `;q=` params, with no platform reordering (the q-value is advisory for the caller).
- **Redirect mid-reroute**: a `call_failure` hop may return `Redirect`, abandoning the hunt.

## Considered alternatives

- A route list walked locally in the b2bua. It adds a replicated field and diverges from the HTTP
  model.
- Magic `From`/`To` keys in the flat header map. They hide identity rewrites in a map meant for
  additive headers.
- The decision owns the full header set. It breaks the generator's Via/CSeq/Call-ID/branch/tag
  invariants.
- Separate new-call and failover enums. The plan's `on_exhausted` is a treatment, so the sets are
  identical.

Detail: [appendix, Considered alternatives](appendix/0017-http-call-treatment-and-header-ownership.md#considered-alternatives).

## Consequences

- No HA or codec migration (X4). Failover transparency (ADR-0014) holds: the plan is replicated
  state, not a runtime dependency on the decision service being reachable during takeover or
  reclaim.
- `apply_route` and `reject_call` carry the decision's headers and identity fields into the
  generators ([appendix](appendix/0017-http-call-treatment-and-header-ownership.md#where-the-treatment-is-wired)).
