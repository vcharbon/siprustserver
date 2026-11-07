# Frozen ABNF corpus

Port of `portsource/sipjsserver/scripts/abnf-fuzz`. The conformance suite
(`tests/it/conformance/`) holds the RFC grammars and generates from them; this
corpus is the vendored generation, kept as a fixed input.

- **Grammars** (`grammars/*.abnf`) — vendored **verbatim** from the source
  suite (themselves from RFC 3261 §25.1 + extension RFCs, via
  [xsnpdngv/SIP-ABNF](https://github.com/xsnpdngv/SIP-ABNF)). Do not edit
  except to re-sync with the source.
- **Corpus** (`corpus/<target>.txt`) — **frozen**, committed, generated once
  by `abnfgen`. The Rust test replays it deterministically — no external
  binary, no run-to-run nondeterminism in CI.
- **Regeneration** (opt-in) — install [`abnfgen`](https://www.quut.com/abnfgen/),
  then `cargo run -p xtask -- abnf-regen [N]` (default N=1000/target).

## What a line's outcome means

`tests/it/abnf_fuzz.rs` wraps each line in a whole message and reads it through
the production parser, then its typed reader:

- **accepted** — the line reads.
- **stated** — the refusal is one a stated rule makes of a form the vendored
  grammar admits (`conformance/stated.rs`).
- **a row** — a refusal of a valid form, or a line that reads but loses an
  obvious field, fails `frozen/<row>`. A row that fails sits on the conformance
  ratchet (`conformance/ratchet.txt`).
