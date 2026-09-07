# RFC rule authoring spec (read before adding or editing a rule)

An RFC-compliance rule has ONE home: `crates/rfc-rules`. It is written against
the wire model and consumed by two adapters — the capture-side census
(`crates/sip-pcap/src/rfc/adapter.rs`) and the live recorded-trace audit
(`crates/sip-net/src/rfc_audit/wire_adapter.rs`). What differs between them is
OBSERVATION POLICY, never rule logic.

## Read these first (they are the spec by example)

- `crates/rfc-rules/src/lib.rs` — the crate contract and the `all_rules()`
  registry.
- `crates/rfc-rules/src/wire.rs` — `Msg` / `WireView` / `Observation`: every
  fact a rule may key on, and `Observation::absence_decidable` for the rules
  whose offence is an absence.
- `crates/rfc-rules/src/verdict.rs` — `RuleId` (the closed kebab-case
  vocabulary, `ALL` and the census `WIRE` subset), `Decision`, `Evidence`,
  `Finding`, `Population`.
- `crates/rfc-rules/src/rules/mod.rs` — the family index and the `Obligation`
  trait. One file per obligation family; a family's rules share one wire walk.
- Family exemplars: `rules/ack.rs` (a `Reading` shared by the two halves of one
  obligation pair, with the emitter/taker split), `rules/branch.rs` (a derived
  reading several FAMILIES consume, living in its own module).

## The interface

```rust
pub trait Obligation: Send + Sync {
    fn id(&self) -> RuleId;
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
5. **Ids are `RuleId` variants**, kebab-case on the wire. Add the variant, add
   the body to `all_rules()` — the registry test fails on either alone. The
   census `WIRE` subset does NOT grow here: graduating a rule into the pivot
   vocabulary is a separate change with its own corpus census run.
6. **Doc comments are contracts**: `/// **RFC XXXX §Y — <one-line MUST>.**`
   then which party is charged, what discharges the obligation, and what is
   undecidable and why.

## Adapter policies (where a finding is surfaced, not decided)

- **Live** (`wire_adapter.rs`): one `WireView` per bind, `Observation::closed`
  true (absence windows collapse). `at_vantage` keeps only violated,
  non-`relayed` findings and applies one of three vantage policies —
  `surfaced` (charged at the emitter bind), `surfaced_at_taker` (the bind whose
  PEER is judged), `surfaced_at_either_end` (a two-party negotiation). Each
  merged rule gets a thin `CrossMessageAuditRule` shim naming its policy and
  rendering the finding text; register the shim in `cross_rules()`.
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
- Registration in `RuleId::ALL` and `all_rules()`, plus the consuming adapter.
