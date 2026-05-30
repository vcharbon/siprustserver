# ABNF fuzz suite (Rust port)

Port of `portsource/sipjsserver/scripts/abnf-fuzz`. Asserts the per-header
parsers never reject a grammar-valid input and never silently misparse one.

- **Grammars** (`grammars/*.abnf`) — vendored **verbatim** from the source
  suite (themselves from RFC 3261 §25.1 + extension RFCs, via
  [xsnpdngv/SIP-ABNF](https://github.com/xsnpdngv/SIP-ABNF)). Do not edit
  except to re-sync with the source.
- **Corpus** (`corpus/<target>.txt`) — **frozen**, committed, generated once
  by `abnfgen`. The Rust test replays it deterministically — no external
  binary, no run-to-run nondeterminism in CI.
- **Regeneration** (opt-in) — install [`abnfgen`](https://www.quut.com/abnfgen/),
  then `cargo run -p xtask -- abnf-regen [N]` (default N=1000/target).

## Rejection classification (ported)

Each rejection is classified as in the source driver:
- **accepted** — grammar-valid input the parser accepted.
- **policy** — rejection matching a documented ADR-0007 semantic rule
  (port out of range, leading-zero octet, missing Via magic cookie, …).
  Expected; the grammar is intentionally looser than the parser here.
- **buggy** — a rejection matching no known policy. A non-zero count is a
  real parser bug (over-narrow stop set, wrong pivot, …).
- **silentMisparse** — accepted but the parsed structure is obviously wrong.

A clean run has `buggy == 0` and `silentMisparse == 0` across all targets.
