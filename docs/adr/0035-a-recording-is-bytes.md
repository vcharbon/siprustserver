# A recording is bytes; a frozen body compares byte for byte

**Status:** accepted (2026-09-19)

## Context

A datagram is bytes from the socket to the run bundle to the confrontation. The
interpreter used to record it as text — a lossy UTF-8 decode — so a body that
was not UTF-8 reached the bundle with replacement characters, could not be
confronted with its resource, and the schema grew a second frozen mode and a
media-type rule to say which payloads were "binary". The capture extractor had
already solved the same problem for the flows document: one lossless form,
chosen from the bytes.

## Decision

Four invariants, held in Rust, the TypeScript contracts and the pipeline alike.

1. **Bytes are the truth.** A datagram is `Vec<u8>` from the socket to the
   recording to the confronter; text is a rendering, never the stored form,
   and no recording site or reader decodes a wire lossily.
2. **One on-disk encoding, the extractor's.** A recorded line carries its
   datagram in exactly one of three arms chosen by the bytes alone — `raw`
   when the whole datagram is UTF-8, `head` + `body_b64` when only the body
   is not, `raw_b64` when not even the head is — beside the body's layout
   (media type, length, MIME parts located by offset), decoded by one
   function for captures and recordings alike.
3. **Comparison is byte-equal, under one bound.** The recorded layout's
   length is THE body every comparison reads, single and multipart, under
   every compare mode: a frozen body or part compares bytes, `sdp` and `xml`
   fold both sides after a strict UTF-8 decode, a multipart expectation
   compares part by part (media type, then bytes) located by that layout.
4. **Nothing names a content type for binary versus text.** There is one
   frozen mode; whether a payload is text is a fact of its bytes, read at
   emission, at recording and at comparison.

## Consequences

- The arm keeps the WHOLE datagram, a tail past the declared `Content-Length`
  included (RFC 3261 §18.3 discards it; the wire carried it). The layout is
  the parser's, `Content-Length`-bounded.
- The head ends by one rule wherever the arm, the layout or a reader measures
  it: the empty line that ends the header block (RFC 3261 §7), CRLF being the
  terminator (§25.1), a bare CR or LF accepted as the parser accepts them.
- The interpreter writes a layout for every parsable datagram carrying a body,
  so a line with no layout is bodiless in the confrontation and the cut; the
  cut stores a body under the same bound, so a resource never holds a tail the
  layout excludes.
- A layout its bytes cannot honour is refused on decode in Rust and stated as
  one probe in the confronter, never thrown.
- The body check selector `body` observes text where the bytes are UTF-8 and
  standard base64 otherwise; `body.b64` observes base64 always. A probe's two
  sides render alike: text where both are text, base64 on both otherwise.
- Resource files are bytes on both sides of the pipeline; a writer never
  re-encodes from a string.
- A viewer that reads bundles without the contracts package keeps its own
  three-arm reader — the one exception to "one decoder", accepted for a
  browser core that validates nothing. It reads the same arms, the same layout
  and the same head-end rule, and it draws what the wire carried: the body
  under a stated layout with the bytes past it marked as excess, the whole
  tail where no layout is stated.
