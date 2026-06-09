# RFC audit rule port — shared authoring spec (read before implementing rules)

You are porting RFC-compliance audit rules from the TypeScript reference
(`/home/vince/sipjsserver/tests/harness/rules/rfc/*`) into Rust
(`/home/vince/siprustserver/crates/sip-net/src/rfc_audit/*`). The framework,
helpers, and two worked exemplars already exist and are green. Follow them exactly.

## Read these first (they are the spec by example)
- Peer-rule exemplar: `crates/sip-net/src/rfc_audit/starter_peer.rs` (`BranchPrefixRule`).
- Cross-rule exemplar: `crates/sip-net/src/rfc_audit/cross_generic.rs` (`MidDialogUriRule`).
- Helpers you MUST reuse (do not re-derive):
  - `crates/sip-net/src/rfc_audit/dialog_model.rs` — message accessors
    (`from_tag`, `to_tag`, `from_uri`, `to_uri`, `call_id`, `cseq_method`,
    `cseq_seq`, `status`, `top_via_branch`, `msg_headers`), the `DialogModel` +
    `advance_dialog_model` + `is_in_dialog_request`, the projector
    `project_per_dialog` → `Vec<DialogSlice{ per_agent: Vec<AgentSlot{ bind_key, ordered: Vec<OrderedEvent{kind,msg,wire_peer}} > }>`,
    `slot_is_relay`, `route_is_loose`, `extract_route_uri`, `parse_sdp_origin`.
  - `crates/sip-net/src/rfc_audit/txn_correlation.rs` — `build_branch_index`,
    `BranchIndex`/`BranchEntry`/`IndexedMessage`, `find_invite_by_branch`,
    `find_request_by_branch`, `has_final_response_for`, `first_response_status_for`,
    `header_values`, `split_option_tags`, `cseq_pair`.
  - `crates/sip-net/src/rfc_audit/offer_answer.rs` — `parse_sdp_body` → `SdpDoc{media: Vec<MediaLine{r#type,port,transport,formats,attributes,c_line,ptime}>, t_line, origin, ..}`,
    `extract_format_list`, `extract_direction` → `SdpDirection`, `extract_rtpmaps`.

## Trait shapes (in `crates/sip-net/src/contracts.rs`)
```rust
pub trait PeerAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> { all_ua_roles() } // override only if the TS rule narrows
    fn force_advisory(&self) -> bool { false }               // true ONLY for the TS-advisory rules
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], bind_key: &str) -> Vec<String>;
}
pub trait CrossMessageAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> { all_ua_roles() }
    fn force_advisory(&self) -> bool { false }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)>; // LaneKey = String bind
}
```
`UaRole` is `Uac|Uas|Proxy`; build a narrowed subject with
`HashSet::from([UaRole::Uas])` (import `crate::types::UaRole` / `all_ua_roles`).

## Hard conventions
1. **Parse leniently.** Use `super::lenient_parser()` (NOT `CustomParser::new()`),
   so grammar-violating wire bytes are still inspectable. The exemplars do this.
2. **Rule name** = `rfcXXXX.<lowerCamel>` derived from the TS `rfc.<x>` name, where
   XXXX is 3261/3262/3264 per the source file (e.g. TS `rfc.tags` in the 3261 set →
   `"rfc3261.tags"`; `rfc.rseqMonotonic` → `"rfc3262.rseqMonotonic"`). Keep it stable —
   it is the public id used by `Harness::allow_violation` and the report.
3. **Per-UA dialog cross rules MUST skip relay slots**: `if slot_is_relay(slot) { continue; }`
   before walking a slot's dialog model (a transparent proxy relays both directions
   of one Call-ID and would false-positive). The `MidDialogUriRule` exemplar shows it.
4. **Attribution / direction.** Peer rules: judge the direction the TS rule judges
   (a rule about a header the sender mints → SENT messages; a "this bind received a
   bad X" → RECEIVED). Cross rules: emit the `bind_key` the TS attributes the finding to.
5. **Conservative.** When the correlating request/transaction was never observed,
   SKIP (no finding) — mirror the TS "cannot judge" guards. Never emit a false positive.
6. **force_advisory.** Set `true` (with a doc-comment justification copied from the TS
   advisory note) ONLY for the rules the TS marks `severityOverride: "advisory"` (the
   B2BUA-architectural divergences: SDP origin continuity, rport echo, OPTIONS-echo,
   no-target-404, reliable-needs-client-optin, unmatched-PRACK-proxied, cancel-after-1xx,
   proxy-100-within-T1, no-new-offer-while-pending, prack-offer-answer-model,
   direction-pair-valid, zero-port-propagation, and any other the source tags advisory).
   Every other rule is a hard-gate rule (default `force_advisory = false`).
7. **Retransmit / fork awareness.** Reuse the patterns in `cseq.rs` (top-Via branch =
   retransmit key; To-tag distinguishes forks). Do not double-count a retransmission.
8. **Doc comments**: match the density/voice of the exemplars — each rule gets a
   `/// **RFC XXXX §Y — <one-line MUST>.** <why a real UA enforces it and the test UA
   masks it>` block. This is a 0-warning, well-documented codebase.

## Each rule needs
- A unit struct + `impl` of the right trait.
- A `#[cfg(test)]` test (add to the module's `tests` mod) with a tiny event vector
  (copy the `req`/`resp`/`recv_at`/`sent_at` byte-builders from the exemplars or
  `cseq.rs`) asserting BOTH a clean case and a flagged case.
- Registration: add `Arc::new(YourRule)` to the module's `peer_rules()` / `cross_rules()`.

## Do NOT
- Do not run `cargo` (another agent may be mid-edit in the same crate — builds would
  race). Write correct code by following the exemplars precisely; integration compiles centrally.
- Do not edit any file other than your assigned module file.
- Do not edit `mod.rs`, `dialog_model.rs`, `txn_correlation.rs`, `offer_answer.rs`,
  `cseq.rs`, or another group's file.
