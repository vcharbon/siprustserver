# TODO — Canonical `sip-message::Method` (independent prerequisite)

**Status:** not started. **Blocks:** ADR-0016 X9 leg-message effect (the
state-machine side-effect taxonomy assumes this has landed).

## Problem

SIP methods are represented three incompatible ways across the workspace:

- `sip_message::generators::InDialogMethod` — `Bye, Invite, Prack, Notify,
  Options, Info, Update, Message, Refer`
- `sip_message::generators::OutOfDialogMethod` — `Invite, Options, Message,
  Register, Subscribe, Publish`
- bare `String` — `SipRequest.method`, `SipResponse` cseq method, `Call.method`,
  and most rule/matcher code.

The two enums overlap (`Invite`/`Options`/`Message`), neither is complete
(no `Cancel`/`Ack`), and the stringly-typed paths re-parse methods ad hoc. There
is **no single canonical method type owned by the sip stack and shared by every
crate** — the smell ADR-0016 X9 surfaced.

## Goal

One `sip-message::Method` enum (the lowest crate, so every crate can depend on
it), covering in- **and** out-of-dialog methods, used wherever a method is named.
`InDialogMethod` / `OutOfDialogMethod` become either views/subsets of it or are
retired; `String` method fields migrate to `Method` (with a permissive
`Other(String)` escape hatch only if an extension method must round-trip).

## Why independent

It touches every crate's method handling (parse, match, generate, CDR), so it is
a stack-wide refactor with its own review surface — kept out of the ADR-0016
effect work so neither blocks the other. Do this **first**; then ADR-0016's
`Effect::LegMessage { method: Method, label }` references it directly.
