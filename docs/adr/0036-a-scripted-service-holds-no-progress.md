# A scripted HTTP service holds no progress; the token is the position

**Status:** accepted (2026-09-22)

## Context

`http-net::scripted` serves the HTTP exchanges a test states as a program:
an instance opens on a first request and follows its steps as the peer comes
back. Where the instance stands has to live somewhere. A counter inside the
service breaks as soon as a peer serves several instances at once, retransmits
a request, or outlives the test run that started a call: the counter says one
thing, the request says another, and nothing on the wire tells which is right.

A peer does not always return an opaque value in the shape it received it.
Some peers carry it inside a structure of their own (encoded, compressed,
nested in another document) and hand it back in that form.

## Decision

1. **The token is the position.** The service mints an opaque token into
   every reply that names a next step (`${continuation}`) and reads it back
   from the next request body. The token carries the service's nonce, the
   instance, the position and the captures (or a code step's state), under a
   checksum. A tokened request is matched on the token alone. Per instance the
   service keeps two monotone facts: `opened`, which the open-match rule reads
   so an opened instance is never opened again, and how far it got, which only
   the verdict reads.
2. **The token may travel wrapped.** A continuation codec
   (`HttpContinuationCodec`) wraps the token where the reply template places
   it and unwraps candidates from a request body before the scan. The raw scan
   always runs too; the default codec wraps nothing. The codec sees the reply
   body it stands in (its continuation empty), so a wrapping may echo the
   reply's own fields; it never carries state of its own, so the service holds
   no progress either way.

## Consequences

- Concurrent instances cannot interfere, and a retransmitted tokened request
  is answered identically: same token, same step, same reply. A repeated
  OPENING request carries no token: it opens the next identical unopened
  instance, or is unmatched when none is left (POST is not idempotent).
- A token minted by an earlier run reaching a new service (a standing peer
  finishing an old call) is told apart by its nonce and reported as an
  advisory, never matched.
- Changing the token's wire form or the codec contract invalidates every
  token in flight on a peer that outlives the run; that is the cost of
  reversing this decision.
- Request fragments (`contains`) match the body as the peer sent it. An
  unwrapped candidate is scanned for tokens only, never matched against.
