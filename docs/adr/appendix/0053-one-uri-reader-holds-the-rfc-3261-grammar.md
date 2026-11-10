# Appendix: One URI reader holds the RFC 3261 grammar

The decision part is [ADR-0053](../0053-one-uri-reader-holds-the-rfc-3261-grammar.md).

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

An opt-in strict pass over the From, To, Contact and Date values held a few more
rules, and no production path ran it.

## Decision

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
  `IPv6address` in the RFC 4291 text form (RFC 5954 §4.1 replaces the §25.1
  rule), with no zone ID, plus the one form RFC 5118 §4.10 has a parser
  tolerate: three colons directly before a dotted-quad IPv4 tail, kept as
  written and read as `::` wherever an address is derived from the host. A port is 1 to 65535. The same host reader
  reads a Via sent-by, whose host or `]` is followed only by a port, LWS, `;`
  or `,`.
  A parameter name is `paramchar`, and so is the value of an `other-param`;
  §25.1 gives `transport`, `user` and `method` a `token` form
  (`other-transport`, `other-user`, `Method`) beside the `other-param` form
  every parameter also has, so either reads there: a token (`transport=%`)
  or a `paramchar` run (`user=a:b`, `transport=x/y`). A `maddr` of a URI a
  request is sent to is a `host`.
  Other values are not checked against their parameter: `ttl=256` is an
  `other-param` by the grammar. A parameter name appears once (§19.1.1): a
  second copy of a value equal as §19.1.4 compares (case aside, an escaped
  unreserved character equal to itself) reads as one copy, the first, which
  the URI renders (ADR-0001, one parameter of a name), and a second value
  refuses the URI. Ingress refuses the collapsed copy too unless a deployment
  exempts it; no exemption names a Request-URI.
- **`tel`**: the number up to the first `;` or `?` as the host, then
  `;params?headers`, no RFC 3966 grammar. A route entry of one reads; a
  consumer that must route on it answers 416 Unsupported URI Scheme. **Every other scheme** (`urn`,
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
Headers the parser does not read are not newly checked. A method is
case-sensitive (§7.1): it keeps the case it was written in, its CSeq names it
in the same case, and a known method is its upper-case spelling alone, so
`invite` is an extension method.

A header line ends with CRLF and nowhere else, in and out: the parser refuses
a bare CR or LF anywhere in the header block, and no writer puts a CR or LF
inside a line (`src/write_check.rs`). Any other control byte but HTAB stands in
a header value only as the escaped byte of a quoted-pair, in a quoted-string or
comment its header's grammar has (a header the stack does not know reads as
holding quoted-strings), and nowhere in a reason phrase
(`src/control_bytes.rs`). Whitespace is SP and HTAB alone: a value is trimmed
of them and of nothing else (`src/whitespace.rs`). A header block ends with
its blank line (§7); one that ends without it is refused.

A header block that is not UTF-8 (RFC 3629) is refused, except a well-formed
surrogate pair, re-encoded; no other byte is replaced. A pair written as two
adjacent three-byte forms (CESU-8, `ED A0-AF 80-BF ED B0-BF 80-BF`) reads as
the one character it encodes, in UTF-8, wherever it stands, and is then
ordinary non-ASCII: read where the grammar takes `UTF8-NONASCII`, refused in a
URI, a tag, a Call-ID, a branch, a method or a header name. The message's
`image()` stays the bytes that arrived, and what the stack sends is rendered
from its lines, so the wire carries the UTF-8 form. A byte no UTF-8 form
admits, a Latin-1 byte among them, is refused as invalid. A lone or reversed
half, an overlong form and a five- or six-byte form, which RFC 3261's
`UTF8-NONASCII` admits, stay refused too, an exception to the goal that every
valid message parses: their reading is in doubt. Accepted risks: a quoted Digest `username` holding a pair is relayed in
its UTF-8 form, which does not match the credentials the sender computed
over its bytes, and a peer comparing its From bytes with our echo sees the
UTF-8 form (`src/parser/custom/mod.rs`).

### 4. The name-addr

`NameAddr::parse_as` reads `( name-addr / addr-spec ) *( SEMI generic-param )`
whole:

- a display name is a quoted-string or tokens separated by LWS, and a quoted
  one is followed by `<`;
- no whitespace inside `<...>`, and a `<` is closed;
- after `>` only `;`-parameters, each `token [ = token / host /
  quoted-string ]` and each name once, then the end;
- a bare addr-spec ends at the first `;`, and is refused in the route family
  (Route, Record-Route, Path, Service-Route), whose `lr` a bare URI would give
  away. In Contact, From and To it carries no `,` or `?` (§20 brackets them
  there). In any other header a `?` starts its URI's headers, and a `,` is
  its own only in a single-valued one (Refer-To, Referred-By, Reply-To): a
  list header's `,` separates entries. RFC 3515 erratum 4652, which would
  bracket Refer-To too, is Held for Document Update, not Verified;
- a header whose grammar has no header parameters (P-Asserted-Identity and
  P-Preferred-Identity, RFC 3325 §9.1) reads a bare value as one URI whole,
  its `;`-parameters the URI's, and nothing after a `>`;
- a bare From or To whose `;` comes before an `@` outside any quoted-string,
  once the strict read fails, reads its URI up to that `@` and the next `;`:
  every typed reader reads it, and the ingress gate refuses it unless the
  deployment names `bare-user-semicolon` (ADR-0001, exemptions).

Every header grammar is read by one reader: one quoted-string rule (a
quoted-pair escapes one ASCII byte but CR and LF), one token scan, one
parameter reader (`header/params.rs`), one Via reader (`Via::read`) and this
name-addr reader. The parameter reader reads strictly, up to its first
parameter off the grammar (a Reason's cause stays readable past it), or
leniently.

### 5. Where it applies

On wire bytes, a URI or name-addr that does not read refuses the message for
the Request-URI, From, To, Contact, Route and Record-Route. Route and
Record-Route are read at parse time, except on a final of 300 or more, which
establishes no dialog (§12.1.1): there, like its Contact, they are not read.
The Contact of an INVITE, SUBSCRIBE, NOTIFY or REFER, and of a 1xx or 2xx to
one, names a dialog's remote target and is a SIP or SIPS URI (§8.1.1.8,
§12.1.1); elsewhere a Contact of any scheme reads.
Identity headers that fail the grammar (P-Asserted-Identity,
P-Preferred-Identity, Diversion, History-Info, Remote-Party-ID, Refer-To …)
are a non-fatal `Err`, and the entry is not used. A header read on access uses
the same reader and returns the error.

There is no opt-in strict pass. The one exception to "strict wherever the
parser validates" is the Date: a malformed Date never refuses the message
(RFC 4475 §3.1.2.12 lets an element ignore it), and nothing reads it.

### 6. The lenient mode

`SipParserLimits::wire_grammar = false` is the reading of a stored image, of a
message the stack assembles from its own parts, and of a recorded exchange an
audit judges: a mode of the one reader of each grammar, not a second
implementation. A value the strict reading takes reads the same
way; one it refuses keeps its scanned parts: an open quote or `<` runs to the
end, parameters read on past a byte off their grammar, a URI the reader refuses
is held opaque instead of refusing the message, and a Via sent-by the reader
refuses is held whole as the host, with no port, and renders as written. A
final of 300 or more keeps its lenient Contact on wire bytes too (§5). The Request-URI is read in every mode: a URI that does not read
is no request's target, whoever built the request.

A stored image reads through `sip_message::read_stored`: this mode with every
limit off and the value gates off, no setting, so a node reads what another
stored whatever either states.

Each reading has a named constructor on `CustomParser`, and a parser is built
only through one, so every site states what it reads:

| role | constructor | limits |
|---|---|---|
| a deployment's wire | `ingress(limits)` | what its configuration states, validated at startup |
| a message built rather than received, read back by a harness or a test party | `built()` | the default bounds, no exemption, the grammar held strictly |
| a stored image | `read_stored(image)` | this mode, every limit off |
| a tool's reading of captured traffic | `tool(limits)`, or a `ToolReading` | what the tool's own configuration states |

The configuration is data, validated once at the edge (`main`, a command
line, a file) and injected by constructor: a process builds its ingress
parser at its composition root (the worker's `Front::parser`, the proxy's
`ProxyConfig::parser`) and hands it to every stage that reads the wire, the
proxy's core, health probe and dead notifier included. The roles are
constructors on one type rather than a newtype each, because consumers depend
on the `SipParser` seam and a test decorates the parser behind it; the role is
decided where the parser is built. `CustomParser::new` and
`CustomParser::with_limits` stay for tests. `clippy.toml` lists both, which
the panic-sites pass refuses in every member's library and binary code. It
lists `ingress`, `built`, `tool` and `ToolReading`'s constructors too, which it
refuses in a tiered crate unless the site states why under
`#[expect(clippy::disallowed_methods, reason = …)]`: the composition root for
`ingress`, a read of what the stack built for `built`; a tool reading belongs
to no production crate (ADR-0051 §2).

A tool's reading may admit more than any deployment. `ToolReading` holds the
exemptions and the bounds on a line, a Request-URI, a header block and a
datagram, stated as `key = value` lines (`sipflow --reading FILE`) and refused
whole on a line that does not read; with none stated a tool reads
`ToolReading::plain_rfc`. A harness that replays captured traffic reads its
recording, and its parties read the wire, under the reading its lane states
(the interpreter's `Lane::reader`); upstream's lanes state `built()`.

### 7. Header cardinality

A single-valued header (RFC 3261 §7.3.1: a header whose grammar is no comma
list) stands on one line of a message, compact forms counted. The headers are
the ones `sip_message::header::SINGLE_VALUED` states by hand, each row citing
its grammar, errata applied.

Every way a message is made holds it. The header-block split refuses a second
line in every reading, `read_stored` included; `hydrate_request` refuses one;
a draft holding one does not freeze (`IncompleteDraft::Repeated`, checked once
per `freeze`, `freeze_bytes` and relay refreeze), so a stack defect costs its
message rather than reaching the wire.

What a service states is text from outside the stack, and its repeat never
costs the message. A single-valued header a service states on more than one
line, names compared as the header they name (casing and compact forms
counted: `Subject`, `subject` and `s` are one), is left out whole, as though
not stated, and the skip is reported as stated text the wire cannot carry
(`StatedTextSkipped`). `write_check::one_line_each` decides it, at
every path a service states lines on: a route's header updates on the leg it
dials, the lines of an authored final (the answer a service mints from a
delivered final included), of an in-dialog request or re-INVITE a service
originates, of a relayed response a transform stamps and of a `DestroyLeg`'s
CANCEL or BYE, and a decision's header statements restated on a built message
(`write_check::header_lines`). As though never stated means what it says: a
line the message would carry with no statement, relayed or the stack's own,
rides.

The cardinality is in the types. A value type declares the header it is
(`HeaderValue::HEADER`, from which `header_name` reads) and its `Lines`
(`header::lines::Single`, or the list layouts `Comma`, `Opaque`, `SetPerLine`).
Its impl must state `HELD`, which only `held_to_the_table!()` gives: its
evaluation holds `Lines` to the table's statement of `HEADER`, and reading the
header's name evaluates it, so a disagreeing impl does not build wherever the
type is used (a `cargo check` does not evaluate it; a build does). A kind is
checked where it is declared. A
single-valued header reads as `msg.header::<H>()`, its one value, and is
written with `Draft::set`; a list header reads as `msg.list::<H>()`. A list
whose values stand in order (`Comma`, `Opaque`) also reads as `first::<H>()`,
its first value from the lines up to it; a token set (`SetPerLine`) reads as
`token_set::<H>()`, the one set its lines make. A list header is appended
with `push` and `prepend`. No accessor exists for the other cardinality.
