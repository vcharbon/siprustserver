# A refused request is answered when it can be

**Status:** accepted (2026-10-22)

**Owner:** `crates/sip-message` (`src/error.rs`, `src/parser/custom/partial.rs`,
`src/parser/custom/headers.rs`, `src/generators/invalid_request.rs`,
`src/header/warning.rs`), `crates/sip-txn` (`src/parse_refusal.rs`,
`src/layer/parse_refusal.rs`), `crates/sip-proxy` (`src/parse_refusals.rs`,
`src/core/request/parse_refusal.rs`).

Amends ADR-0051 (a parse failure drops the packet).

## Context

A request the parser refused was dropped silently at both ingresses, the
transaction layer's and the proxy's stateless path. Its sender retransmits
until Timer B or F, then reports a timeout, where RFC 3261 lets the element
answer 400 (§8.2.6 for a UAS, §16.3 for a proxy; §18.3 for a datagram shorter
than its Content-Length) as long as it can build the response. Building it
needs the fields a response echoes (§8.2.6.2): Via, From, To, Call-ID, CSeq.

## Decision

### 1. The error carries what a response needs

`SipParseError { reason, partial }`. `reason` names the first rule the message
broke, as before; there is no error kind. `partial` is present exactly when a
response can answer the request correctly:

- the request line parses, and the request is no ACK (§17: an ACK draws no
  response);
- every Via entry, the From, the To, the Call-ID and the CSeq pass the parse's
  full rules (all Vias are echoed, so all must read), and the CSeq names the
  request line's method (a response naming another matches no client
  transaction, §17.1.3);
- no limit stopped the parse.

It holds the method, the five fields typed, and their lines as received.

### 2. The header split reads past a refused line

A header line that breaks the grammar or a gate beyond it (ADR-0001) is left
out and the split reads on, so the five fields after it are still read; the
first refused line is the one reported. A limit (the header length) stops the
split at once and leaves no partial: a message past a limit is not worth
reading further. A refused line naming Via, From, To, Call-ID or CSeq leaves
no partial either, since the field it held is not echoed, and so does a refused
line whose name did not read, which may have been any of them.

### 3. The answer

`400 Invalid Request`, built in `sip-message` from the partial alone: Via,
From, To, Call-ID and CSeq echoed byte for byte, the top Via stamped with the
packet's source (`received`, a requested `rport`: §18.2.1, RFC 3581 §4), a
To-tag added to a To that has none, and `Warning: 399 <agent> "<reason>"`. The
agent is the element's advertised `hostport` (the receiving face's advertised
address on the proxy; the configured advertised address in the transaction
layer, else the address the 400 leaves from). The warn-text keeps ASCII
letters, digits, space and `.,:;-_()/=+'@[]<>`, every other byte becomes `?`,
and it stops at 200 bytes: no byte of a reason can close the quoted-string or
the header line, and no escaping is needed. No body.

The 400 is stateless: no server transaction, sent to the datagram's source
address and port. Every copy of the request is answered again under the same
To-tag (derived from the request with a per-instance secret, §8.2.6.2). The
ACK to the 400 ends at the element that sent it: the transaction layer
recognises its derived To-tag on an ACK matching no transaction, the proxy
absorbs it as the ACK to its own non-2xx final.

### 4. What is never answered

A response, an ACK, a request without a partial, a limit refusal: each is
dropped as before.

### 5. One cap per ingress

A token bucket of 100 tokens refilling at 100 a second caps the 400s of one
ingress (the transaction layer's owner, the proxy's recv shards together), so a
flood of malformed requests whose source may be forged draws a bounded rate of
answers. A datagram that no 400 can answer never touches the cap. The
counters: answered, dropped by the cap, dropped unanswered
(`*_parse_refusals_answered_total`, `*_parse_refusals_capped_total`,
`*_parse_errors_total`), in the transaction layer and the proxy alike.

## Considered and rejected

- **An error kind hierarchy, and accepting a message minus its bad optional
  header.** Costs a classification of every rule for traffic that is rare; the
  parser is made stricter instead (ADR-0053), and a refusal is answered or
  dropped, never repaired.
- **A server transaction for the 400.** It would hold state for traffic that
  is malformed by definition; a stateless answer to every copy costs one
  bucket token per copy and nothing else.
- **Remembering the refusal to answer copies.** Each copy is re-parsed and
  re-answered with the same bytes; a memo would only save the build.
- **Escaping the reason in the warn-text.** Quoted-pairs and `%XX` keep every
  byte but add a decoding rule a reader must know; replacing what is not plain
  text keeps the reason readable and the header safe.

## Consequences

- A sender of a readable but refused request learns it at once, with the rule
  named in the Warning, instead of timing out.
- Reflection is bounded: a 400 is the request's echoed headers plus a status
  line, a To-tag, a Warning of at most about 240 bytes and `Content-Length: 0`,
  and the cap bounds how many are sent.
