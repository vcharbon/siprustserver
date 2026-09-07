# Correlation rule format v0

Guidelines for issue 01. Iterated freely during the prototype; whatever survives
becomes the schema frozen in issue 03.

## Design stance (user ruling)

Simple from the rule-writer's point of view, even if that costs implementation
effort. Five easy-to-understand rule kinds beat three cleverer generic ones. The
extension path is: add a sixth kind with its own small matcher, never make an
existing kind more general. If a family cannot be expressed, write it in
`issues/friction.md`; do not grow the rule language on the spot.

Explicit anti-goals:
- No predicate AST, no boolean composition of rules, no quantifiers. The upstream
  `sip-pcap` query module is NOT the model here.
- No configurable anchors, scopes, or projections per rule. Each kind hard-codes
  its anchor and states it in one sentence.
- No first-wins suppression. Every rule fires independently; ambiguity is
  reported, never silently resolved.

## Layering (ruled 2026-08-20)

Three layers, and this rule file is ONLY the middle one:

1. **In-dialog assembly** (same Call-ID + tags → dialog; dialogs → legs; B2BUA
   pairing): deterministic SIP semantics, done upstream in Rust (`sipflow`)
   before TS sees the document. Never expressed as rules.
2. **Cross-call correlation** (this file): membership + chains, purely
   mechanical — tokens, status classes, windows. A rule carries NO meaning; its
   name is an evidence label. Identity extraction is CALL-level: a call's
   identities are those appearing on ANY INVITE it sends, across all its legs
   (D1 ruling — a call correlated once contributes all its messages' data).
3. **Interpretation / shape detection** ("486 then a new downstream call within
   seconds = reroute-on-busy"; family classification; pivot `routing.attempts`
   causes): downstream JS code (LogicExtractor / CaseAssembler) consuming the
   neutral chains. Causes are derived from chain evidence (recorded status),
   never from rule names.

## Input unit

A "call" is one call group as emitted by `sipflow --json` (schema 5). Upstream
already did generic correlation (B2BUA leg pairing, icid, derived Call-ID). The
rules here join call groups into cross-call cases: reroute chains, transfers,
deployment-specific loops.

## File shape

```json
{
  "version": 0,
  "rules": [ { "name": "…", "kind": "…", "…": "kind-specific fields" } ]
}
```

Every rule has `name` (unique, shows up in evidence) and `kind`. Unknown fields
are an error. Regex is the only power feature: where a kind takes a regex, the
regex must define a named group `key`; two calls join when their extracted keys
are equal.

## The five kinds

### 1. `call-id`
Derived Call-ID relationship. Anchor: each call's Call-ID (any leg).

```json
{ "name": "b2bua-term-suffix", "kind": "call-id",
  "left": "^(?<key>.+)$", "right": "^term\\d+-(?<key>.+)$" }
```

Join when a left call's key equals a right call's key. No window: the derivation
itself is the evidence.

### 2. `header-key`
Shared token in a header of the initial INVITE. Anchor: first INVITE of each call.

```json
{ "name": "icid", "kind": "header-key",
  "header": "P-Charging-Vector", "pattern": "icid-value=(?<key>[^;]+)" }
```

Join when both calls yield the same key. Optional `window_ms` between the two
INVITEs (default: none).

### 3. `retry`
Reroute / failover chain. A CALL-LEVEL rule (ruled 2026-08-20, friction F3): it
reads whole-call facts (this call's terminal failure, that call's start), never
individual messages. Anchor: left call's terminal failure, right call's initial
INVITE.

```json
{ "name": "reroute-486", "kind": "retry",
  "finals": ["486", "480", "5xx"],
  "window_ms": 15000,
  "match": ["to-user"] }
```

`finals`: status patterns (`"486"` exact or `"5xx"` class) that must terminate
the left call. `match` is OPTIONAL:
- With `match` absent, retry fires only between calls ALREADY in the same group
  through other rules' joins (call-id generation, header-key, refer). This is
  the primary form: membership comes from stronger evidence, retry contributes
  the chain — order and cause. It never joins strangers, so dial-form identity
  mismatches (`33000900001` vs its `+<trunk>CCNSN` form) are irrelevant.
- With `match` present (subset of `from-user | to-user | ruri-user`), retry may
  itself join, for captures where the attempts share no other token. Each name
  denotes the call-level SET of D1; two calls match when their digits-normalized
  sets intersect, and the evidence names the matched value and the INVITE each
  side read it from.

A final joins the NEXT candidate INVITE only: the earliest one starting after it
within `window_ms` (ruled 2026-08-20, friction F1 — a chain of N attempts is
N-1 ordered joins forming a list, not N² pairs; the order is what pivot
`routing.attempts` consumes). Two INVITEs equally "next" within the window are
an ambiguity, kept and flagged as usual.

Rule-writing hazard (ruled 2026-08-20, friction F3): D1 identity sets include
B-leg destinations, and some of those are shared platform targets (e.g. an MRF)
reached by many unrelated calls. A `to-user` match through such a target can
join strangers if `finals` is generous — keep `finals` tight, and when a join
surprises you check the evidence's `initial: false` flag. The engine stays dumb
on purpose: no frequency suppression, no initial-INVITE preference. Note the
legitimate case: an MRF leg that truly belongs to a call joins via Call-ID
derivation (layer 2), and layer-3 downstream JS classifies it as MRF from its
INFO-with-XML exchange — identity match was never the intended path for it.

### 4. `replaces`
Attended-transfer linkage. Anchor: right call's initial INVITE carrying
`Replaces` naming the left call's dialog (Call-ID + tags).

```json
{ "name": "replaces", "kind": "replaces" }
```

No fields. The header is the evidence.

### 5. `refer`
REFER-initiated call. Anchor: a REFER on the left call; right call's initial
INVITE whose R-URI (or To) user matches the Refer-To target user
(digits-normalized), starting within `window_ms` of the REFER.

```json
{ "name": "refer-follow", "kind": "refer", "window_ms": 10000 }
```

## Engine contract

Two phases (introduced by the F3 ruling):
1. **Join phase**: every rule except match-less `retry` yields pairwise joins;
   union-find closes them into groups.
2. **Chain phase**: `retry` rules run over each group's calls (and, when they
   carry `match`, across groups — those joins feed back into membership),
   pairing each qualifying terminal failure with the next attempt. Chain joins
   carry ordered positions; they are what pivot `routing.attempts` is built
   from.

- Each rule independently yields pairwise joins:
  `{ left, right, rule, evidence }` where evidence carries the matched key /
  status / header value and the observed `dt_ms`.
- Groups are the union-find closure over all joins.
- Ambiguity: when one call joins more than one candidate under the same rule
  (same key three ways, two INVITEs in one retry window), keep every join,
  flag `ambiguous: true`, and list them in a dedicated output section. Ambiguity
  is a decision point for a human or a later rule, never an engine choice.
- Output (one JSON doc per input flows doc): `calls`, `joins`, `groups`,
  `ambiguities`, `chains` (each ladder's calls in attempt order — the direct
  input to pivot `routing.attempts`; ruled 2026-08-20). Inspectable with jq;
  this file is the prototype's whole API.
- The engine is pure data in, groups out: no deployment constants in the
  engine. A deployment's rules live in a separate committed rule file.
