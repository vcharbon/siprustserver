# RFC rule authoring spec (read before adding or editing a rule)

An RFC-compliance rule has ONE home: `crates/rfc-rules`. It is written against
the wire model and consumed by two adapters — the capture-side census
(`crates/sip-pcap/src/rfc/adapter.rs`) and the live recorded-trace audit
(`crates/sip-net/src/rfc_audit/wire_adapter.rs` and its per-family modules).
What differs between them is OBSERVATION POLICY, never rule logic.

## Read these first (they are the spec by example)

- `crates/rfc-rules/src/lib.rs` — the crate contract and `all_rules()`.
- `crates/rfc-rules/src/registry.rs` — `RuleId` (the closed kebab-case
  vocabulary; `ALL`, the census `WIRE` set and `RELAYABLE` read off the family
  tables) and the `rules!` table each family declares its rules in.
- `crates/rfc-rules/src/wire.rs` — `Msg` / `WireView` / `Observation`: every
  fact a rule may key on, and `Observation::absence_decidable` for the rules
  whose offence is an absence.
- `crates/rfc-rules/src/verdict/` — `Decision`, `Finding`, `Population`, and
  the `Evidence` vocabulary (`verdict/evidence.rs`).
- `crates/rfc-rules/src/rules/mod.rs` — the family index and the `Obligation`
  trait. One file per obligation family; a family's rules share one wire walk.
- Family exemplars: `rules/ack.rs` (a `Reading` shared by the two halves of one
  obligation pair, with the emitter/taker split), `rules/branch.rs` (a derived
  reading several FAMILIES consume, living in its own module).

## The interface

```rust
pub trait Obligation: Send + Sync {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding>;
}
```

`eval` returns EVERY occasion — `Violated(Evidence)`, `Compliant` and
`Undecidable(reason)` alike — so the population contract
`hits ⊆ decided ⊆ occasions` is the caller's to fold, never to reconstruct.
A `Finding` names the charged `emitter`, the `taker`, whether the emission was
`relayed`, and the `anchor` message it points at.

## Hard conventions

1. **Never parse wire syntax in a rule.** `Msg` carries the keyed facts;
   anything beyond them is read off `Msg::head` / `Msg::body` through
   `sip_message::sniff`, the header accessors, or `sip_message::sdp_doc`.
   `sip-message` is the only home for SIP and SDP grammar.
2. **A rule states an invariant; it does not state a policy.** Conservatism
   (`Undecidable`), relay attribution (`Finding::relayed`) and end-of-evidence
   (`Observation::closed`) are data the adapters weigh. Never branch on which
   consumer is running.
3. **Repeat policy is explicit.** A `Msg::repeat` opens no obligation
   (retransmitting until answered is required behaviour) but may MEET one. Each
   rule's doc says which reading it applies.
4. **A shared walk gets its own module.** A `Reading` that more than one FAMILY
   consumes lives beside `branch.rs`, never inside one family's file.
5. **A rule is declared once, in its family's `rules!` table**: a doc line
   (the RFC clause and the one-line contract) over `Name = "kebab-token";`
   (`, relayable` where a forwarded copy carries the offence). `Name` is both the
   `RuleId` constant and the unit-struct body; `RuleId::ALL`, `all_rules()` and
   the live adapter's suite read the tables. Then `just rule-copies` rewrites the
   copies outside Rust (the token fixture, the TypeScript `RFC_RULES`, the review
   goldens); pivot-schema's `every_rule_list_copy_is_current` names a stale one.
   Removing a rule is the same two steps.
6. **Doc comments are contracts**: `/// **RFC XXXX §Y — <one-line MUST>.**`
   then which party is charged, what discharges the obligation, and what is
   undecidable and why.

## Adapter policies (where a finding is surfaced, not decided)

- **Live** (`wire_adapter.rs`): one `WireView` per bind, `Observation::closed`
  true (absence windows collapse). `at_vantage` keeps only violated,
  non-`relayed` findings at the vantage a rule's `Live` row names — the emitter,
  the taker (the bind whose PEER is judged), either end (a two-party
  negotiation), or the taker else the emitter (an omission the taker is owed) —
  with its skipped lanes, roles, severity and detail line. A family's rows sit in
  `wire_adapter/<family>.rs`; a rule with no row is surfaced at the emitter with
  a generic detail line.
  Lane classification comes from `rfc_audit::relay_lanes`; per-message reads
  from `rfc_audit::msg_reads`.
- **Capture** (`sip-pcap/src/rfc/`): one view per leg, the recording span as the
  observation (conservatism windows live), `relayed` findings KEPT, and
  `census.rs` folding the population statistics. Conformance pins for the merged
  bodies run through this adapter (`rfc/{ack,cancel,prack}.rs`).

## Each rule needs

- A unit struct + `impl Obligation` in its family file.
- A `#[cfg(test)]` test in that family's `tests` mod asserting BOTH a clean case
  and a flagged case, plus the semantics no adapter can state
  (closed-observation behaviour, both halves of a pair, repeat handling).
- Its row in the family's `rules!` table, then `just rule-copies`; a live
  `Live` row where the default surfacing does not fit.
