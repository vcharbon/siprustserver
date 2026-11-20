# Appendix: HTTP call-treatment model, header ownership, and the reroute plan

The decision part is [ADR-0017](../0017-http-call-treatment-and-header-ownership.md).

## The b-leg generator owns the structural headers

The b-leg INVITE generator owns the structural headers (From/To/Via/CSeq/Call-ID/Contact/
Max-Forwards) from typed opts, and the flat header-update map only appends non-structural
headers. A flat `update_headers["From"]` appends a second From header rather than rewriting it,
so a From or To number rewrite is not expressible as a map entry.

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
  branch/tag invariants the generator guarantees, an unacceptable blast radius.
- **Keep separate new-call vs failover enums.** Rejected in favor of one
  `CallTreatment`: less duplication, and the plan's `on_exhausted` is literally a
  treatment, so the sets are identical anyway.

## Where the treatment is wired

- `apply_route` threads `route.update_headers` into `build_b_leg`'s `header_updates` slot, and
  `new_from`/`new_to` into the generator opts.
- `reject_call` threads `update_headers` into the `extra_headers` slot and has a Contact-list
  path for 302 (`response_to_a_leg` has both `contact` and `extra_headers` parameters).
- The decision-engine trait returns the unified type to the scripted adapter, `apply_route` and
  `initial_invite`.
