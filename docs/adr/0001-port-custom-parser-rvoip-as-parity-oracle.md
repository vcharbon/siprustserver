# Port the custom SIP parser whole; keep rvoip-sip-core as a dev-only parity oracle

**Status:** accepted (2026-05-30)

The source ships two parser paths: a hand-written zero-regex TypeScript state
machine (`customParser`) and a native path backed by the `rvoip-sip-core` Rust
crate. We port the **custom parser whole** rather than adopting `rvoip-sip-core`
as the production parser, because the ABNF fuzz suite and the ADR-0007 strict
security gate (the 9 grammar rules) are both built around the custom parser's
per-header functions — porting it whole keeps the entire test corpus a 1:1
functional port. `rvoip-sip-core` is retained as a **dev-only second
implementation** behind the `rvoip-oracle` feature, driving the compliance
matrix as a looser lexical cross-check.

## Considered and rejected

- **Adopt rvoip-sip-core as the production parser + re-derive the ADR-0007
  gate.** Would orphan the ABNF tests (which target custom-parser functions)
  and re-author the documented security boundary — more risk, less test
  fidelity.
- **rvoip strict mode alone, drop the 9-rule gate.** Abandons the ADR-0007
  security boundary and its PROTOS/secusiptest corpus.

## Consequences

- The custom parser is significant porting work (scanner, start-line, headers,
  structured-headers, extract-fields), but each Rust module is a direct port
  of a named source file.
- The compliance matrix must encode where `CustomParser` is intentionally
  *stricter* than the rvoip oracle (ADR-0007): a custom rejection where rvoip
  accepts is only valid if it maps to a documented policy rule — reusing the
  source driver's policy-vs-buggy classification. The oracle never silently
  accepts something the custom parser must reject.
- `rvoip-sip-core` is never linked into production builds (feature-gated,
  dev-dependency only).
