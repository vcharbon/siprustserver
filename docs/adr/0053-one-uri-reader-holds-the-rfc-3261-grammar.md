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
  starting with a letter) or `IPv4address`; an IPv6 reference holds an
  `IPv6address` as §25.1 writes it or in the RFC 4291 text form (RFC 5954, and
  RFC 5118 §4.10 asks for both), with no zone ID. A port is 1 to 65535.
  Parameter values are not checked against their parameter: `ttl=256` and
  `transport=x` are `other-param`s by the grammar.
- **Every other scheme** (`tel`, `urn`, `im` …): a structural
  `user@host:port;params?headers` reading, no RFC 3966 grammar, and every byte
  of the URI character set (`reserved / unreserved / "%"` and the IPv6
  brackets).
- A Request-URI also carries no escaped headers (§19.1.1 Table 1), and a
  recorded route is held to the same rule.

### 3. The character classes

`char_class` holds the §25.1 classes, one predicate each. All are ASCII:
`UTF8-NONASCII` belongs only in a quoted-string, a comment, a reason phrase and
TEXT-UTF8, so raw UTF-8 refuses a URI in any position and of any scheme. There
is no percent-encoding on egress: what arrives escaped stays escaped, what
arrives raw is refused. On wire bytes the classes also hold the Call-ID
(`word [ "@" word ]`), the From and To tags and the Via branch (`token`), the
CSeq method (`token`) and every header name (`token`). Headers the parser does
not read are not newly checked.

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
Record-Route are read at parse time. The identity headers (P-Asserted-Identity,
P-Preferred-Identity, Diversion, History-Info, Remote-Party-ID, Refer-To) stay
non-fatal: the error is their stored value. A header read on access uses the
same reader and returns the error.

The Date gate of the old opt-in pass joins the parse: a Date not in the
§20.17 form refuses the message. The opt-in pass is gone.

### 6. The lenient mode

`SipParserLimits::wire_grammar = false` is the reading of a message built by
the stack or read by a test harness. From, To and Contact keep their scanned
parts there, and a URI the reader refuses is held opaque instead of refusing
the message. The Request-URI is read in every mode: a URI that does not read
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

- A message the grammar refuses is dropped where it was read before; the
  refusal is the packet's alone (ADR-0051).
- A test or fixture that relied on a URI the grammar refuses carries a
  compliant one; a test whose subject is that input asserts the refusal.
- The vendored ABNF grammars of the fuzz corpus draw a header parameter's name
  from the URI's `paramchar`; the suite counts those refusals as policy.
