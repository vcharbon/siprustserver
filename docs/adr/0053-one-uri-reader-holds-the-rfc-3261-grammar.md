# One URI reader holds the RFC 3261 grammar

**Status:** accepted

**Owner:** `crates/sip-message`: `src/char_class.rs` (the §25.1 character classes),
`src/header/uri/read.rs` (the URI reader), `src/header/name_addr.rs` and `src/header/params.rs`
(the name-addr and its parameters), `src/parser/custom/` (where the parse applies them).

Amends ADR-0001 (the gate list) and ADR-0003 (the opt-in strict pass).

The [appendix](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md) holds the defects
behind the context and the full rules of sections 2 to 7.

## Context

Three readers cut a URI out of the same text, each at its own delimiters, so the URI one of
them validated was not the URI the router took apart. Route and Record-Route were read only on
access, and most URI parts went unchecked.
Detail: [appendix, Context](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#context).

## Decision

### 1. One reader, one cut

`Uri::parse` (outer whitespace aside) and `Uri::read_exact` (the inside of `<...>`, a
Request-URI) are the only URI reader. Validation and the typed value come from the same cut, so
the URI a gate accepts is the URI a router reads, by construction. Every other reader of a URI
goes through it.

### 2. The grammar per scheme

`sip` and `sips` follow the RFC 3261 §25.1 grammar whole, and any byte left over refuses the URI.
The URI's role (`UriRole`) moves one rule: a host that names a party may have a digit-led top
label, a host a request is sent to may not. IPv6 references follow RFC 4291 text form, with the
RFC 5118 §4.10 tolerance. A parameter name appears once. `tel` is read by the character set
only, and every other scheme is opaque. A Request-URI and a recorded route carry no escaped
headers.
Detail: [appendix §2](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#2-the-grammar-per-scheme).

### 3. The character classes

`char_class` holds the §25.1 classes, all ASCII, so raw UTF-8 refuses a URI anywhere. There is no
percent-encoding on egress. The classes also hold the Call-ID, the tags, the Via branch, the
methods and every header name. A method is case-sensitive. A header line ends with CRLF and
nowhere else, and a header block that is not UTF-8 is refused, a well-formed surrogate pair
aside.
Detail: [appendix §3](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#3-the-character-classes).

### 4. The name-addr

`NameAddr::parse_as` reads `( name-addr / addr-spec ) *( SEMI generic-param )` whole. A bare
addr-spec is refused in the route family. Each header grammar has one reader: one quoted-string
rule, one token scan, one parameter reader, one Via reader and this name-addr reader.
Detail: [appendix §4](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#4-the-name-addr).

### 5. Where it applies

On wire bytes, a URI or name-addr that does not read refuses the message for the Request-URI,
From, To, Contact, Route and Record-Route, a final of 300 or more aside. Identity headers that
fail the grammar are a non-fatal `Err`, and the entry is not used. A malformed Date never refuses
the message.
Detail: [appendix §5](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#5-where-it-applies).

### 6. The lenient mode

`SipParserLimits::wire_grammar = false` is a mode of the one reader of each grammar, for a stored
image, a message the stack assembles, and a recorded exchange an audit judges. The Request-URI is
read in every mode. Each reading has a named constructor on `CustomParser` (`ingress`, `built`,
`read_stored`, `tool`), and `clippy.toml` holds where each may be called.
Detail: [appendix §6](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#6-the-lenient-mode).

### 7. Header cardinality

A single-valued header (RFC 3261 §7.3.1) stands on one line, compact forms counted, as
`sip_message::header::SINGLE_VALUED` states. Every way a message is made refuses a second line. A
service's repeated statement of one is left out whole and reported (`StatedTextSkipped`). The
cardinality is in the types, so no accessor exists for the other cardinality.
Detail: [appendix §7](appendix/0053-one-uri-reader-holds-the-rfc-3261-grammar.md#7-header-cardinality).

## Considered and rejected

- **Percent-encode raw UTF-8 on egress.** Rewriting a URI a peer sent changes what a hop compares
  (§19.1.4) and hides the peer's defect.
- **An RFC 3966 grammar for `tel`.** Its users read the subscriber and its parameters. The
  character set is enough to keep a `tel` value from carrying bytes no URI may.
- **Value checks on `ttl`, `transport`, `lr`.** The grammar accepts any `other-param`, and a
  router reads the values it uses.

## Consequences

- A message the grammar refuses is refused where it is read. The refusal is the packet's alone
  (ADR-0051), a request answered 400 when it can be (ADR-0054).
- Capture ingest (`sipflow`) sets aside the messages its reading refuses and copies the accepted
  ones whose non-fatal header reading failed beside them, counting each (`parse_failed`,
  `header_failed`) and each by its reason's key (`SipParseError::reason_key`). A stricter
  grammar's cost on a capture is seen and attributed.
- A test or fixture that relied on a URI the grammar refuses carries a compliant one. A test
  whose subject is that input asserts the refusal.
- The vendored ABNF grammars of the fuzz corpus draw a header parameter's name from the URI's
  `paramchar`. The suite counts those refusals as policy.
