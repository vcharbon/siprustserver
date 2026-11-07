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

The wire-grammar gates mostly enforce RFC 3261 §25.1 as written, every URI
through one reader (ADR-0053). These refuse what the grammar or the RFC's
compatibility rules admit, on purpose:

- **The magic cookie.** The topmost Via's branch starts with `z9hG4bK`
  (§8.1.1.7) and carries an identifier after it. A scope choice: RFC 2543
  elements are not served, departing from the backward-compatible matching of
  §17.2.3, so RFC 4475 §3.1.1.1, §3.1.1.7 and §3.4.1 are refused, and so is
  §3.2.1's bare cookie.
- **The transport allowlist.** The topmost Via, the hop this element answers,
  names a transport of `SipParserLimits::allowed_transports`. A lower Via may
  name any transport §25.1 admits.
- **Ports.** A port is at most 65535; port 0 is refused on the topmost Via and
  in a URI, and read on a lower Via.
- **IPv4 octets.** An octet of an IPv4 host in a URI or a Via sent-by is at
  most 255 and carries no leading zero (`1*3DIGIT` admits both): an octal
  reading of `010` names another host.
- **One From tag.** A From with two `tag` parameters is refused (§8.1.1.3,
  §19.3).
- **Readable credentials.** An Authorization or Proxy-Authorization whose
  Digest credentials do not read is refused with its message (§22.4).
- **A URI's `maddr`.** A URI a request is sent to or routed on (Request-URI,
  Contact, Route, Record-Route; Refer-To and Reply-To at their typed read) names
  a host in its `maddr`, or has none: `other-param` admits a flag, a port or a
  bracketed name, and none has a target to read.
- **Numeric headers.** A header of the numeric registry carries a decimal value
  within its range, digits only, followed by nothing but the tail its grammar
  has: CSeq's method, a session timer's parameters.

A gate refuses like any other rule: a refused request whose echoed fields read
is answered 400 (ADR-0054).

## A response never follows a Via's `maddr`, and a URI's `maddr` is its target

A Via carrying `maddr` is accepted and relayed, and its `maddr` chooses no
destination. A response goes to `received` and `rport` as the receiving hop
stamped them, else to the sent-by (`Via::response_target`). This departs on
purpose from RFC 3261 §18.2.2, which forwards a response to the `maddr`, and from
the `maddr` condition of RFC 3581 §4.

- **It would let any sender aim our traffic.** The `received` stamp ties a
  response to the address the request came from. A Via's `maddr` is text the
  sender wrote, so honoring it sends each response to whichever address the
  sender names, with no check that the address asked for anything. RFC 3261
  §26.1.5 describes this kind of attack using a falsified source address and Via;
  `maddr` would not even need the source address falsified, so source-address
  filtering would not stop it. A multicast `maddr` would also spread the response
  to a whole group.
- **Its purpose does not arise here.** §18.1.1 has a client add `maddr` when it
  sends a request to a multicast address. This stack joins no multicast group, so
  a request reaching it was sent to a unicast address, and the response target
  already holds what the response needs.

A URI is different. Its host is already chosen by whoever wrote the URI, so its
`maddr` adds no steering where the author of the host wrote it, and RFC 3261 §16.5
and RFC 3263 §4 make it the target.
A URI's `maddr` is honored, at the URI's own port, wherever a URI picks the next
hop (`Uri::next_hop`): a remote target, a Route entry, a decided Request-URI, a
relayed one. It stays on the Request-URI sent onward, and the stack never mints
one. A proxy strips a Request-URI's `maddr` naming itself (§16.4) and takes a Route
naming itself by `maddr` as its own.

An inbound Request-URI on a proxy's external face chooses no destination: there a
sender is asking the proxy to take the request, and following what it wrote would
make the proxy an open relay. The request reaches a worker, which decides what it
sends onward. A deployment whose decision service states the host of a request it
reads hands that service no `maddr` of the caller's: neither honored nor passed on,
so no copied parameter pairs the service's host with the caller's address.

The Via rule matches ADR-0048: of a response's address the sender chooses only the
port, at its sent-by with no `rport`, and the IP stays the packet's source. Serving
multicast SIP would reopen it, and would need a way to check the `maddr` against
the group the request actually arrived on.

## The bounds

A party's new request meets one bound, at the hop it first reaches, and everything
after that hop reads with room. A refusal happens once, where it can be answered:
a message a party's first hop accepts never fails a later read of this stack
because of what a hop legitimately added.

**The first-ingress bound** (`SipParserLimits::first_ingress`) is a stated
exception to the goal that every valid message parses:

- **A header line** is at most 8,192 bytes, `name + ": " + value`, unfolded and
  trimmed.
- **A Request-URI** is at most 8,192 bytes, so a Contact URI that fits its line
  can become the Request-URI of this stack's request.
- **A header block** is at most 100 lines, its start line and fold continuations
  included.

It holds a request with no To-tag at its first hop: the front proxy's external
face, on a proxy with one face a source that is no registry worker, or a worker
with no proxy (`at_first_ingress`). One past it is answered 400 there (ADR-0054).
A request with a To-tag carries what hops added to its dialog's lines, so it
meets no bound of its own.

**The room** (`max_*`) is what every read of the wire is held to. A line and a
Request-URI are bounded by the datagram, 65,535 bytes: a hop adds `received` and
`rport`, a To-tag, and lines a worker writes from a party's values, its Via and
Contact carrying the Call-ID and From tag escaped, which no constant smaller than
the datagram holds. A block has 32 lines past the bound: a forward adds its Via,
a double Record-Route and a Max-Forwards, a worker a preloaded Route and what a
route states. A hop's edit of a comma-folded line keeps it on one line, so no edit
adds a line per value. A message past the room is refused unanswered, with no
partial. A body already reaches the datagram, and the proxy's memo counts the
bytes of what it keeps, so the room costs nothing a party could not spend today.

A process states these once, at startup, and every stage that reads its wire holds
the one parser built from them: the proxy's intake hook and core, a worker's
ingress brake and transaction layer. A stored image is read under none of them
(ADR-0053 §6).

The band this leaves is the datagram. A message this stack builds can pass the
largest UDP payload, 65,507 bytes, though each line it copied fit: a callee's 2xx
whose Contact fills a datagram leaves no room for the ACK and BYE naming that
target, a caller's INVITE near a datagram none for its b-leg with the worker's Via
and Contact, and a callee's 2xx with a long relayed line none on the caller's face.
The socket refuses the send, and the refusal is counted
(`b2bua_txn_send_errors_total`); the transaction runs as if the message were lost,
so its own timer ends it and the call is released with its CDR
(`router::oversize_send_tests`). The party that message was for hears nothing of
it. A b-leg that cannot leave draws the caller's final from the first-response
timer; a 2xx that cannot reach the caller ends on its ladder, and she gives up on
Timer B; a callee left without an ACK ends his side on his 2xx give-up.

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
