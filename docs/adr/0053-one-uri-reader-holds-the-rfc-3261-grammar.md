# One URI reader holds the RFC 3261 grammar

**Status:** accepted (2026-10-21)

**Owner:** `crates/sip-message`: `src/char_class.rs` (the §25.1 character
classes), `src/header/uri/read.rs` (the URI reader), `src/header/name_addr.rs`
and `src/header/params.rs` (the name-addr and its parameters),
`src/parser/custom/` (where the parse applies them).

Amends ADR-0001 (the gate list) and ADR-0003 (the opt-in strict pass).

## Context

Three readers cut a URI out of the same text, each at its own delimiters: a
strict validator that stopped at `;`, `?` or `>`, a Request-URI reader that
stopped at `;,> \t?`, and the typed reader every router reads, which never
checked that it had consumed its input. A URI one of them validated was not
the URI the router took apart:

- the Request-URI `sip:a>b@h` was validated as one URI and routed to host `a`;
  `sip:b@h,evil` was routed to `h`;
- a bare `Route: sip:p;lr` gave its `lr` to the header, so the hop read as a
  strict router;
- Route and Record-Route were read only on access, by the typed reader alone,
  so an empty host or `[::1]junk` passed;
- the userinfo took any byte, escapes were never checked, a host label was
  checked by its first byte, and URI parameters, `?headers` and raw UTF-8 went
  unchecked anywhere in a URI.

A second, opt-in strict pass over the From, To, Contact and Date values held a
few more rules, but no production path ran it.

## Decision

### 1. One reader, one cut

`Uri::parse` (outer whitespace aside) and `Uri::read_exact` (the inside of
`<...>`, a Request-URI) are the only URI reader. Validation and the typed value
come from the same cut, so the URI a gate accepts is the URI a router reads,
by construction. Every other reader of a URI goes through it.

### 2. The grammar per scheme

- **`sip` / `sips`**: the RFC 3261 §25.1 grammar whole. Every byte after the
  scheme lands in the userinfo, the host, the port, a URI parameter or an
  escaped header, each held to its class; anything left over refuses the URI.
  The first `@` ends the userinfo, since no later part admits one. Hosts are
  `hostname` (labels of alphanumerics and inner hyphens, the top label
  starting with a letter) or `IPv4address`; a dotted run of digits is an
  `IPv4address` or no host. The reader takes the URI's role
  (`UriRole`) as a parameter, and the role moves one rule only: a host that
  names a party and is never resolved (From, To, the identity headers) may
  have a digit-led top label (RFC 1123 §2.1), so `27` and `1x.27` are
  hostnames there. A host a request is sent to or routed on (Request-URI,
  Contact, Route, Record-Route, Via, Refer-To) keeps the RFC 3261 top label,
  so no resolver reads `2130706433`, `0x7f.0.0.1` or `0177` as an IPv4
  literal. An IPv6 reference holds an
  `IPv6address` in the RFC 4291 text form only (RFC 5954 §4.1 replaces the
  §25.1 rule), with no zone ID. A port is 1 to 65535. The same host reader
  reads a Via sent-by, whose host or `]` is followed only by a port, LWS, `;`
  or `,`.
  Parameter values are not checked against their parameter: `ttl=256` and
  `transport=x` are `other-param`s by the grammar.
- **`tel`**: the number up to the first `;` or `?` as the host, then
  `;params?headers`, no RFC 3966 grammar. **Every other scheme** (`urn`,
  `http`, `cid` …) is opaque: its scheme-specific part is kept whole, and it
  names no user, host, port, parameter or header; a route-family entry of one
  names no hop and is refused. Both are held to every byte of the URI character
  set (`reserved / unreserved / "%"` and the IPv6 brackets), plus a raw `#` in
  a `tel` number, and neither the number nor the opaque part is empty.
- A Request-URI also carries no escaped headers (§19.1.1 Table 1), and a
  recorded route is held to the same rule.

### 3. The character classes

`char_class` holds the §25.1 classes, one predicate each. All are ASCII:
`UTF8-NONASCII` belongs only in a quoted-string, a comment, a reason phrase and
TEXT-UTF8, so raw UTF-8 refuses a URI in any position and of any scheme. There
is no percent-encoding on egress: what arrives escaped stays escaped, what
arrives raw is refused. On wire bytes the classes also hold the Call-ID
(`word [ "@" word ]`), the From and To tags and the Via branch (`token`), the
request-line and CSeq methods (`token`) and every header name (`token`).
Headers the parser does not read are not newly checked. A method keeps the case
it was written in (§7.1): an extension method is never uppercased, and its CSeq
names it in the same case; a known method matches in any case.

A header line ends with CRLF and nowhere else, in and out: the parser refuses
a bare CR or LF anywhere in the header block, and no writer puts a CR or LF
inside a line (`src/write_check.rs`).

### 4. The name-addr

`NameAddr::parse_as` reads `( name-addr / addr-spec ) *( SEMI generic-param )`
whole:

- a display name is a quoted-string or tokens separated by LWS, and a quoted
  one is followed by `<`;
- no whitespace inside `<...>`, and a `<` is closed;
- after `>` only `;`-parameters, each `token [ = token / host /
  quoted-string ]`, then the end;
- a bare addr-spec ends at the first `;`, carries no `,` or `?` (§20), and is
  refused in the route family (Route, Record-Route, Path, Service-Route),
  whose `lr` a bare URI would give away.

### 5. Where it applies

On wire bytes, a URI or name-addr that does not read refuses the message for
the Request-URI, From, To, Contact, Route and Record-Route. Route and
Record-Route are read at parse time, except on a final of 300 or more, which
establishes no dialog (§12.1.1): there, like its Contact, they are not read.
Identity headers that fail the grammar (P-Asserted-Identity,
P-Preferred-Identity, Diversion, History-Info, Remote-Party-ID, Refer-To …)
are a non-fatal `Err`, and the entry is not used. A header read on access uses
the same reader and returns the error.

The opt-in strict pass is gone. The one exception to "strict wherever the
parser validates" is the Date: a malformed Date never refuses the message
(RFC 4475 §3.1.2.12 lets an element ignore it), and nothing reads it.

### 6. The lenient mode

`SipParserLimits::wire_grammar = false` is the reading of a message built by
the stack or read by a test harness. From, To and Contact keep their scanned
parts there, and a URI the reader refuses is held opaque instead of refusing
the message; a Via sent-by the reader refuses is held whole as the host, with
no port, and renders as written. The Request-URI is read in every mode: a URI that does not read
is no request's target, whoever built the request.

## Considered and rejected

- **Percent-encode raw UTF-8 on egress.** Rewriting a URI a peer sent changes
  what a hop compares (§19.1.4) and hides the peer's defect.
- **An RFC 3966 grammar for `tel`.** Its users read the subscriber and its
  parameters; the character set is enough to keep a `tel` value from carrying
  bytes no URI may.
- **Value checks on `ttl`, `transport`, `lr`.** The grammar accepts any
  `other-param`, and a router reads the values it uses.

## Consequences

- A message the grammar refuses is refused where it was read before; the
  refusal is the packet's alone (ADR-0051), a request answered 400 when it can
  be (ADR-0054).
- Capture ingest (`sipflow`) sets aside the messages it refuses and copies the
  accepted ones whose non-fatal header reading failed beside them, counting
  each (`parse_failed`, `header_failed`), so a stricter grammar's cost on a
  capture is seen, not silent.
- A test or fixture that relied on a URI the grammar refuses carries a
  compliant one; a test whose subject is that input asserts the refusal.
- The vendored ABNF grammars of the fuzz corpus draw a header parameter's name
  from the URI's `paramchar`; the suite counts those refusals as policy.
