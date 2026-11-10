# Port the custom SIP parser whole; keep rvoip-sip-core as a dev-only parity oracle

**Status:** accepted (2026-05-30)

The source ships two parser paths: a hand-written zero-regex TypeScript state
machine (`customParser`) and a native path backed by the `rvoip-sip-core` Rust
crate. We port the **custom parser whole** rather than adopting `rvoip-sip-core`
as the production parser, because the ABNF fuzz suite and the strict
wire-grammar gates (`SipParserLimits::wire_grammar`) are both built around the
custom parser's per-header functions. Porting it whole keeps the entire test
corpus a 1:1 functional port. `rvoip-sip-core` is retained as a **dev-only
second parser** behind the `rvoip-oracle` feature: a differential test reads
each message through both and compares what they read.

Each gate's full rule with its RFC sections, the `maddr` rationale and the
datagram band are in the
[appendix](appendix/0001-port-custom-parser-rvoip-as-parity-oracle.md), under
the same headings.

## The gates beyond the grammar

The wire-grammar gates mostly enforce RFC 3261 §25.1 as written, every URI
through one reader (ADR-0053). These refuse what the grammar or the RFC's
compatibility rules admit, on purpose:

- **No RFC 2543 compatibility.** A shape an RFC keeps only for RFC 2543 is
  refused like any other off-grammar shape.
- **The magic cookie.** The topmost Via's branch starts with `z9hG4bK`, in any
  case, and carries an identifier after it.
- **The transport allowlist.** The topmost Via names a transport of
  `SipParserLimits::allowed_transports`.
- **Ports.** A port is at most 65535; port 0 is refused on the topmost Via and
  in a URI.
- **IPv4 octets.** An octet is at most 255 with no leading zero.
- **Status codes.** A status code is 100 to 699.
- **One parameter of a name.** A parameter name appears once in a header value
  and in a URI; a repeat with a different value is always refused.
- **One line of a single-valued header.** A header of `header::SINGLE_VALUED`
  appears once, compact forms counted.
- **Readable credentials.** An Authorization or Proxy-Authorization that
  `Credentials::parse` does not read is refused.
- **What a request states.** A request carries a From tag and a Max-Forwards;
  Contact and To-tag rules follow dialog establishment.
- **Exemptions.** `SipParserLimits::exemptions` (`SIP_EXEMPTIONS`, empty by
  default) names each shape with one reading a deployment accepts against the
  RFC, consulted only by the ingress gate.
- **A URI's `maddr`.** A URI a request is sent to or routed on names a host in
  its `maddr`, or has none.
- **Numeric headers.** A numeric header carries a decimal value within its
  range, then only its grammar's tail.
- **A body a reader can frame.** A body states its Content-Type, and a
  multipart one has exactly one framing reading.
- **Empty values.** An empty Via, Contact, Route, Record-Route, Content-Type or
  Content-Disposition line, and an empty comma-list entry, is refused.

A gate refuses like any other rule: a refused request whose echoed fields read
is answered 400 (ADR-0054).
Detail: [appendix](appendix/0001-port-custom-parser-rvoip-as-parity-oracle.md#the-gates-beyond-the-grammar).

## A response never follows a Via's `maddr`, and a URI's `maddr` is its target

A response goes to `received` and `rport`, else to the sent-by
(`Via::response_target`), never to a Via's `maddr`. This departs on purpose
from RFC 3261 §18.2.2 and RFC 3581 §4: a Via's `maddr` would let any sender
aim this stack's traffic, and this stack joins no multicast group. A URI's
`maddr` is honored at the URI's own port wherever a URI picks the next hop
(`Uri::next_hop`), as RFC 3261 §16.5 and RFC 3263 §4 state. An inbound
Request-URI on a proxy's external face chooses no destination.
Detail: [appendix](appendix/0001-port-custom-parser-rvoip-as-parity-oracle.md#a-response-never-follows-a-vias-maddr-and-a-uris-maddr-is-its-target).

## The bounds

A party's new request meets one bound, at the hop it first reaches, and
everything after that hop reads with room. The first-ingress bound
(`SipParserLimits::first_ingress`) caps a header line and a Request-URI at
8,192 bytes and a header block at 100 lines, on a request with no To-tag at
its first hop; one past it is answered 400. The room (`max_*`) bounds every
read by the datagram, 65,535 bytes, plus 32 block lines; past it a message is
refused unanswered. A message this stack builds that cannot leave in one
datagram is a result: its own request fails before the socket, a relayed
in-dialog request is answered 513, a response stays a counter.
Detail: [appendix](appendix/0001-port-custom-parser-rvoip-as-parity-oracle.md#the-bounds).

## Considered and rejected

- **Adopt rvoip-sip-core as the production parser + re-derive the gates.**
  Would orphan the ABNF tests (which target custom-parser functions) and
  re-author the documented security boundary: more risk, less test fidelity.
- **rvoip strict mode alone, drop the gates.** Abandons the security boundary
  and its PROTOS/secusiptest corpus.

## Consequences

- Each Rust parser module (scanner, start-line, headers, structured-headers,
  extract-fields) is a direct port of a named source file.
- The differential test must encode every place `CustomParser` and the rvoip
  oracle part, each with its RFC section: a custom rejection where rvoip
  accepts is only valid if it maps to a gate above or to grammar rvoip does
  not enforce, and a field the two read differently is a deviation of one of
  them. The oracle never silently accepts something the custom parser must
  reject.
- `rvoip-sip-core` is never linked into production builds (feature-gated, refused
  in production closures), and no workspace build turns the feature on: the
  `just parity-oracle` recipe builds and runs it.
