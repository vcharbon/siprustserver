# Port the full RFC post-call-treatment rule suite to the Rust SIP harness

## Context

The TypeScript reference server (`/home/vince/sipjsserver`) audits **every call test by
default** against a 73-rule RFC-compliance suite (RFC 3261 / 3262 / 3264) that runs over the
recorded SIP wire trace at layer close. These are *post-call-treatment* rules: they validate
that the on-wire behaviour the B2BUA produced obeys protocol invariants that a real UAC/UAS
would enforce but the **test peers (Alice/Bob) do not** — the test UAs answer whatever they
are handed (any CSeq, any tag, any Route), so without the audit a B2BUA bug is silently
masked by a compliant-looking `expect(200)`.

The Rust port (`/home/vince/siprustserver`) already has the **entire audit framework** — the
`PeerAuditRule` / `CrossMessageAuditRule` traits, subject dispatch, `force_advisory`,
`should_audit_bind`, and default wiring through `Harness::finish()` — but ships only **3
rules** (the in-dialog CSeq family in `crates/sip-net/src/rfc_audit.rs`). The other ~70 RFC
invariants are absent, so the Rust call tests are far less protected than the TS ones.

**Goal:** port the full 73-rule suite (full fidelity), wire all of it on by default for every
SIP call test, then run the whole call-test suite and resolve every new finding — fixing
Alice/Bob behaviour where the *test peer* was non-compliant, and delegating a B2BUA fix where
the rule caught a *real bug*. Rules are only ever downgraded (advisory/exception) for the
B2BUA-architectural divergences the TS suite itself marks advisory, or for a fixture that
**deliberately** sends non-compliant SIP as a voluntary non-compliance simulation — never to
silence a genuine bug.

## Authoritative source (TS) → Rust target map

| TS source file | Rule count | Rust target |
|---|---|---|
| `tests/harness/rules/rfc/starter-peer-rules.ts` | 17 | `sip-net/src/rfc_audit/starter_peer.rs` |
| `tests/harness/rules/rfc/rfc3261-peer-rules.ts` | 4 | `sip-net/src/rfc_audit/rfc3261_peer.rs` |
| `tests/harness/rules/rfc/rfc3262-peer-rules.ts` | 2 | `sip-net/src/rfc_audit/rfc3262_peer.rs` |
| `tests/harness/rules/rfc/rfc3264-peer-rules.ts` | 2 | `sip-net/src/rfc_audit/rfc3264_peer.rs` |
| `tests/harness/rules/rfc/cross-message-rules.ts` | 8 | `sip-net/src/rfc_audit/cross_generic.rs` |
| `tests/harness/rules/rfc/rfc3261-cross-message-rules.ts` | 19 | `sip-net/src/rfc_audit/rfc3261_cross.rs` |
| `tests/harness/rules/rfc/rfc3262-cross-message-rules.ts` | 14 | `sip-net/src/rfc_audit/rfc3262_cross.rs` |
| `tests/harness/rules/rfc/rfc3264-cross-message-rules.ts` | 9 | `sip-net/src/rfc_audit/rfc3264_cross.rs` |
| helper `_dialog-model.ts` | — | `sip-net/src/rfc_audit/dialog_model.rs` |
| helper `_transaction-correlation.ts` | — | `sip-net/src/rfc_audit/txn_correlation.rs` |
| helper `_offer-answer.ts` | — | `sip-net/src/rfc_audit/offer_answer.rs` (reuse `sip-message/src/sdp.rs`) |

The existing 3 CSeq rules stay (they correspond to the TS `rfc.cseq` / response-CSeq /
ACK-CSeq area). `rfc_audit.rs` becomes the module dir `rfc_audit/` with a `mod.rs`.

## Architecture of the port

The framework already exists — this is **breadth, not new infrastructure**. Reuse, do not
reinvent:

- **Rule traits** — `PeerAuditRule` and `CrossMessageAuditRule` in
  `crates/sip-net/src/contracts.rs:120-142`. Each rule is a unit struct implementing
  `name()`, optional `subject()` (role dispatch), optional `force_advisory()`, and `check()`.
  Follow the exact shape of the three existing rules in `rfc_audit.rs`.
- **Message access** — `sip_message::message_helpers` (`get_header`, `get_headers`,
  `extract_tag`, `extract_name_addr_uri`, `parse_via_params`, `parse_uri_params`,
  `extract_host_port`) plus the typed `SipRequest`/`SipResponse` fields
  (`.cseq`, `.from`, `.to`, `.via`, `.contacts`, `.method`, `.status`, `.body`, `.headers`).
- **SDP** — reuse `sip_message::sdp` for the RFC 3264 rules; the `offer_answer.rs` helper only
  needs to lift m=/c=/t=/o= lines and codec lists (mirror `_offer-answer.ts`).
- **Per-dialog projection** — port `projectPerDialog()` into `dialog_model.rs`: slice the flat
  `[Stamped<SignalingNetworkEvent>]` into per-`(bind, Call-ID, From-tag, To-tag)` dialog
  slices with To-tag migration and fork handling, exactly as the TS does. The cross-message
  rules iterate these slices; the existing CSeq rule already implements the same
  stream/fork/To-tag keying inline and is the reference for the projector.
- **Transaction correlation** — port `_transaction-correlation.ts` into `txn_correlation.rs`:
  a top-Via-branch → {requests, responses} index, used by the ~8 rules that compare a
  CANCEL/ACK/PRACK against the INVITE it relates to.

### Default wiring (apply to every call test)

1. **`sip_net::rfc_cross_message_rules()`** (`rfc_audit/mod.rs`) — extend to return *all*
   cross-message rules (generic + 3261 + 3262 + 3264 + the existing 3 CSeq rules).
2. **New `sip_net::rfc_peer_rules()`** — returns all peer rules.
3. **`crates/scenario-harness/src/agent.rs:194-197`** — set
   `ScopedAuditOptions { rules: sip_net::rfc_peer_rules(), cross_message_rules:
   sip_net::rfc_cross_message_rules(), .. }`. This installs the suite on the recording layer.
4. **`crates/scenario-harness/src/report/mod.rs:33`** — already pulls
   `rfc_cross_message_rules()`; extend to include peer-rule findings in the rendered report.

### Hard-gate generalization (the actual test-failure path)

`RunReport.audit` is informational — a call test fails **only** via the `finish()`
`panic!` (`agent.rs:344-347`, function `rfc_cseq_findings` at `agent.rs:360`). Today that gate
runs only cross rules and ignores `force_advisory`/`subject`. Generalize it
(rename to `rfc_hard_gate_findings`) so it is the single authoritative gate for the whole
suite:

- Build `bind_roles: HashMap<LaneKey, HashSet<UaRole>>` from the `BindAcquire { summary }`
  events in the snapshot (self-contained; no harness-private state needed).
- Run **peer rules** per-bind (slice events by `bind_key`, like the `Drop` path in
  `contracts.rs:495-522`) and **cross rules** globally.
- **Skip** any finding from a `force_advisory()` rule, and any finding whose rule `subject()`
  does not intersect the originating bind's roles.
- Panic on the remaining (non-advisory, subject-matched) findings — these fail the test.

Advisory findings still flow to the recorder/report via the existing `close()` path, so they
remain visible without failing the run. The `Harness` `Drop` cseq backstop
(`agent.rs:500`) is updated to call the same generalized function.

### Per-test voluntary-non-compliance hook

For fixtures where Alice/Bob **deliberately** emit non-compliant SIP (negative-path testing),
add an opt-in on `Harness`:

```rust
h.allow_violation("rfc3261.<rule>", "Bob deliberately sends BYE outside dialog to exercise 481 path");
```

It records `(rule_name, justification)` and downgrades exactly that rule to advisory for that
run (the gate skips it; the report still shows it tagged with the justification). This is the
only sanctioned way to deactivate a rule in a test, and it requires a written justification.
Default behaviour (no call) = fully gated.

### Global advisory set (B2BUA-architectural divergences)

Port the TS `force_advisory` tag verbatim for the ~14 rules the reference marks advisory
because the B2BUA legitimately diverges per-leg (PRACK/offer-answer termination across legs,
media anchoring, loopback-no-NAT rport, OPTIONS-keepalive response headers, fresh `o=` per
side, etc.). Each carries the TS justification string. **During triage, an advisory finding is
still investigated**: if it reflects a real B2BUA bug rather than the documented architectural
divergence, fix the B2BUA instead of leaving it advisory.

## Implementation order

1. **Scaffold** `rfc_audit/` module dir: move the 3 existing CSeq rules into
   `rfc3261_cross.rs` (or keep a `cseq.rs`), add `mod.rs` with `rfc_cross_message_rules()` +
   `rfc_peer_rules()`. Keep `sip_net` re-exports stable so `agent.rs` imports don't churn.
2. **Helpers**: `dialog_model.rs` (per-dialog projector), `txn_correlation.rs` (branch index),
   `offer_answer.rs` (SDP lift over `sip_message::sdp`). Unit-test each helper.
3. **Peer rules** (25): starter (17) + 3261 (4) + 3262 (2) + 3264 (2). Each with the TS
   subject and a focused unit test mirroring the TS rule's intent.
4. **Cross-message rules** (50): generic (8) + 3261 (19) + 3262 (14) + 3264 (9), tagging the
   advisory ones. Unit-test each with a minimal recorded-trace fixture (follow the
   `recv_at`/`req`/`resp` builders already in `rfc_audit.rs` tests).
5. **Wire defaults** (`agent.rs`, `report/mod.rs`) and **generalize the hard gate**
   (advisory + subject aware), add `Harness::allow_violation`.
6. **Run the call-test suite and triage** (next section).

## Triage-and-fix loop (one pass, all green)

Run the full SIP call-test surface:
`cargo test -p sip-net -p scenario-harness -p b2bua -p b2bua-harness -p sip-txn -p sip-proxy`
(the 31 `b2bua-harness/tests/*.rs`, `sip-txn/tests`, proxy routing tests, etc.).

For each newly-failing test, classify the finding into exactly one remedy (the TS
`RFC_Verification.md` triage policy):

1. **Test-peer (Alice/Bob) non-compliance** → fix the harness peer behaviour in
   `crates/scenario-harness/src/agent.rs` (the `Agent`/`Dialog` send paths) so the peer emits
   compliant SIP. Most likely for Route/Contact/Max-Forwards/tag echoing the simple test UA
   omits.
2. **Real B2BUA bug** → delegate a subagent to fix it in `crates/b2bua` (rules/relay/timers),
   with the failing rule + trace as the spec. Do not weaken the rule.
3. **Deliberate non-compliance fixture** → add `h.allow_violation(rule, justification)` to
   that one test (only when Alice/Bob are *intentionally* violating).
4. **Genuine B2BUA-architectural divergence** → confirm it matches the documented TS advisory
   justification; keep the rule `force_advisory`. Investigate first — never use this to mask a
   bug.

Iterate until `cargo test` is green across all the crates above with the full suite on by
default.

## Files to modify (primary)

- **New:** `crates/sip-net/src/rfc_audit/{mod,starter_peer,rfc3261_peer,rfc3262_peer,rfc3264_peer,cross_generic,rfc3261_cross,rfc3262_cross,rfc3264_cross,dialog_model,txn_correlation,offer_answer}.rs`
- **Replace:** `crates/sip-net/src/rfc_audit.rs` → folded into the dir above.
- **Edit:** `crates/sip-net/src/lib.rs` (module + re-exports), `crates/scenario-harness/src/agent.rs`
  (default `rules`, generalized hard gate, `allow_violation`), `crates/scenario-harness/src/report/mod.rs`.
- **Edit (triage):** `crates/scenario-harness/src/agent.rs` Alice/Bob send paths and/or
  `crates/b2bua/src/rules/*` per finding; per-test `allow_violation` calls in
  `crates/b2bua-harness/tests/*.rs` only for deliberate-violation fixtures.

## Verification

1. `cargo test -p sip-net` — every rule's unit test passes (each rule self-tests clean +
   flag cases, per the existing `rfc_audit.rs` pattern).
2. `cargo test -p b2bua-harness -p scenario-harness -p sip-txn -p b2bua -p sip-proxy` — the
   whole call-test suite is green with the full suite gating by default.
3. `cargo clippy --all-targets` clean, no new warnings (the repo holds a 0-warning bar).
4. Spot-check: temporarily break a B2BUA behaviour (e.g. reuse a CSeq) and confirm a call
   test now panics with the rule's message — proving the gate is live, then revert.
5. Confirm `allow_violation` downgrades exactly its named rule and nothing else (unit test on
   the harness).

## Risks / notes

- **Scale:** 73 rules + 3 helpers is a large body of code. Each rule is small; the bulk is the
  per-dialog projector and transaction-correlation helpers (porting `_dialog-model.ts` and
  `_transaction-correlation.ts` faithfully is the gating work — the existing CSeq rule is the
  reference for the projection keying and the fork/To-tag/retransmit handling hazards).
- **Real bugs likely:** turning the suite on may surface genuine B2BUA defects (the recent
  PRACK+UPDATE forking and reclaim-BYE history suggests dialog/CSeq/route edges). Those are
  fixed via subagent, not silenced — that is the point of the exercise.
- **No `Date::now`/`Math::random`** in this code path; all timestamps come from the recorded
  `at_ms`, so the audit stays deterministic under the paused clock.
- **Keep `sip_net` re-export surface stable** so `scenario-harness` and the failover matrix
  harness (which run their own bind-scoped CSeq audit and call `disarm_cseq_gate`) keep
  compiling; the generalized gate must preserve the `disarm` semantics.
