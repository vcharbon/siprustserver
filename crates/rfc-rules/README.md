# rfc-rules: authoring guide

Read this before adding or editing a rule. The crate contract itself is in `src/lib.rs`.

An RFC-compliance rule has one home: this crate. It is written against the wire model and
consumed by two adapters: the capture-side census (`crates/sip-pcap/src/rfc/adapter.rs`) and
the live recorded-trace audit (`crates/sip-net-testkit/src/rfc_audit/wire_adapter.rs` and its
per-family modules). They differ in observation policy, never in rule logic.

## Read these first (they are the spec by example)

- `src/lib.rs`: the crate contract and `all_rules()`.
- `src/registry.rs`: `RuleId` (the closed kebab-case vocabulary; `ALL`, the census `WIRE`
  set and `RELAYABLE` read off the family tables) and the `rules!` table each family declares
  its rules in.
- `src/wire.rs`: `Msg` / `WireView` / `Observation`, every fact a rule may key on, and
  `Observation::absence_decidable` for the rules whose offence is an absence.
- `src/verdict/`: `Decision`, `Finding`, `Population`, and the `Evidence` vocabulary
  (`verdict/evidence.rs`).
- `src/rules/mod.rs`: the family index and the `Obligation` trait. One file per obligation
  family; a family's rules share one wire walk.
- Family exemplars: `rules/ack.rs` (a `Reading` shared by the two halves of one obligation
  pair, with the emitter/taker split), `rules/branch.rs` (a derived reading several families
  consume, in its own module).

## The interface

```rust
pub trait Obligation: Send + Sync {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding>;
}
```

`eval` returns every occasion, `Violated(Evidence)`, `Compliant` and `Undecidable(reason)`
alike, so the population contract `hits ⊆ decided ⊆ occasions` is the caller's to fold, never
to reconstruct. A `Finding` names the charged `emitter`, the `taker`, whether the emission was
`relayed`, and the `anchor` message it points at.

## Hard conventions

1. **Never parse wire syntax in a rule.** `Msg` carries the keyed facts. Anything beyond them
   is read off `Msg::head` / `Msg::body` through `sip_message::sniff`, the header accessors,
   or `sip_message::sdp_doc`. `sip-message` is the only home for SIP and SDP grammar.
2. **A rule states an invariant, not a policy.** Conservatism (`Undecidable`), relay
   attribution (`Finding::relayed`) and end-of-evidence (`Observation::closed`) are data the
   adapters weigh. A rule never branches on which consumer runs it.
3. **Repeat policy is explicit.** A `Msg::repeat` opens no obligation (retransmitting until
   answered is required behaviour) but may meet one. Each rule's doc says which reading it
   applies.
4. **A shared walk gets its own module.** A `Reading` that more than one family consumes
   lives beside `branch.rs`, never inside one family's file.
5. **A rule is declared once, in its family's `rules!` table**: a doc line (the RFC clause
   and the one-line contract) over `Name = "kebab-token";` (`, relayable` where a forwarded
   copy carries the offence). `Name` is both the `RuleId` constant and the unit-struct body;
   `RuleId::ALL` and `all_rules()` read the tables. The rule also takes its `Live` row in
   `crates/sip-net-testkit/src/rfc_audit/wire_adapter/<family>.rs`
   (`every_merged_rule_has_a_live_adapter` in `wire_adapter/tests.rs` names a rule without
   one). Then `just rule-copies` rewrites the copies outside Rust (the token fixture, the
   TypeScript `RFC_RULES`); pivot-schema's `every_rule_list_copy_is_current` names a stale
   one. Removing a rule is the same three steps.
6. **Doc comments are contracts**: `/// **RFC XXXX §Y — <one-line MUST>.**`, then which party
   is charged, what discharges the obligation, and what is undecidable and why.

## Adapter policies (where a finding is surfaced, not decided)

- **Live** (`sip-net-testkit/src/rfc_audit/wire_adapter.rs`): one `WireView` per bind,
  `Observation::closed` true (absence windows collapse). `at_vantage`
  (`wire_adapter/vantage.rs`) keeps only violated, non-`relayed` findings at the vantage a
  rule's `Live` row names: the emitter, the taker (the bind whose peer is judged), either end
  (a two-party negotiation), or the taker else the emitter (an omission the taker is owed).
  The row also carries the skipped lanes, roles, severity and detail line. An emitter that is
  no recorded bind is judged at the taker, with two exceptions. An emitter-only row (an
  ordering of what it took and sent, which the taker's clock cannot show) never falls back to
  the taker. An emitter-unless-lost row (a message it never sent) falls back to the taker
  except where the taker recorded the loss model dropping its datagrams on that call. Lane
  classification
  comes from `rfc_audit::relay_lanes`, per-message reads from `rfc_audit::msg_reads`.
- **Capture** (`sip-pcap/src/rfc/`): one view per leg, the recording span as the observation
  (conservatism windows live), `relayed` findings kept, and `census.rs` folding the
  population statistics. Conformance pins for the merged bodies run through this adapter
  (`rfc/{ack,cancel,prack}.rs`).

## Each rule needs

- A unit struct and `impl Obligation` in its family file.
- A `#[cfg(test)]` test in that family's `tests` mod asserting both a clean case and a flagged
  case, plus the semantics no adapter can state (closed-observation behaviour, both halves of
  a pair, repeat handling).
- Its row in the family's `rules!` table, its `Live` row in the live adapter's family file,
  then `just rule-copies`.
