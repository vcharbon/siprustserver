# Port the custom SIP parser whole; keep rvoip-sip-core as a dev-only parity oracle

**Status:** accepted (2026-05-30)

The source ships two parser paths: a hand-written zero-regex TypeScript state
machine (`customParser`) and a native path backed by the `rvoip-sip-core` Rust
crate. We port the **custom parser whole** rather than adopting `rvoip-sip-core`
as the production parser, because the ABNF fuzz suite and the strict
wire-grammar gates (`SipParserLimits::wire_grammar`) are both built around the
custom parser's per-header functions — porting it whole keeps the entire test
corpus a 1:1 functional port. `rvoip-sip-core` is retained as a **dev-only
second parser** behind the `rvoip-oracle` feature: a differential test reads
each message through both and compares what they read.

## The gates beyond the grammar

The wire-grammar gates mostly enforce RFC 3261 §25.1 as written. These refuse
what the grammar or the RFC's compatibility rules admit, on purpose:

- **The magic cookie.** The topmost Via's branch starts with `z9hG4bK`
  (§8.1.1.7). A scope choice: RFC 2543 elements are not served, departing from
  the backward-compatible matching of §17.2.3, so RFC 4475 §3.1.1.1 and
  §3.1.1.7 are refused.
- **The transport allowlist.** The topmost Via, the hop this element answers,
  names a transport of `SipParserLimits::allowed_transports`. A lower Via may
  name any transport §25.1 admits.
- **Ports.** A port is at most 65535; port 0 is refused on the topmost Via and
  in a URI, and read on a lower Via.
- **One From tag.** A From with two `tag` parameters is refused (§8.1.1.3,
  §19.3).
- **Readable credentials.** An Authorization or Proxy-Authorization whose
  Digest credentials do not read is refused with its message (§22.4).
- **Numeric headers.** A header of the numeric registry carries a decimal value
  within its range, digits only.

## Considered and rejected

- **Adopt rvoip-sip-core as the production parser + re-derive the gates.**
  Would orphan the ABNF tests (which target custom-parser functions) and
  re-author the documented security boundary — more risk, less test fidelity.
- **rvoip strict mode alone, drop the gates.** Abandons the security boundary
  and its PROTOS/secusiptest corpus.

## Consequences

- The custom parser is significant porting work (scanner, start-line, headers,
  structured-headers, extract-fields), but each Rust module is a direct port
  of a named source file.
- The differential test must encode every place `CustomParser` and the rvoip
  oracle part, each with its RFC section: a custom rejection where rvoip
  accepts is only valid if it maps to a gate above or to grammar rvoip does
  not enforce, and a field the two read differently is a deviation of one of
  them. The oracle never silently accepts something the custom parser must
  reject.
- `rvoip-sip-core` is never linked into production builds (feature-gated, refused
  in production closures), and no workspace build turns the feature on: the
  `just parity-oracle` recipe builds and runs it.
