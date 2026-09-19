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
   recording to the confronter. Text is a rendering — a ladder, a validator
   parse, a text-body compare — never the stored form. No recording site or
   reader decodes a wire lossily.
2. **One on-disk encoding, the extractor's.** A recorded line carries its
   datagram in exactly one of three arms, chosen by the bytes alone: `raw` when
   the whole datagram is UTF-8, `head` + `body_b64` when only the body is not,
   `raw_b64` when not even the head is. The line also states the body's layout
   (media type, byte length, MIME parts located by offset), so a reader never
   splits on a boundary. `sip_message::payload` owns the encoding and the
   layout; `@sip/contracts` `Wire` is the one decoder for captures and
   recordings on the TypeScript side.
3. **Comparison is byte-equal.** A frozen body or part compares bytes. The
   `sdp` and `xml` compares decode both sides as UTF-8 first and apply their
   fold; a side that is not UTF-8 under a text compare is a difference. A
   multipart expectation compares part by part, each part under its own
   `compare`, located by the recording's layout.
4. **Nothing names a content type for binary versus text.** There is one frozen
   mode. Whether a payload is text is a fact of its bytes, read at emission,
   at recording and at comparison; no registry rule says so for a media type.

## Consequences

- The body check selector `body` observes text where the bytes are UTF-8 and
  standard base64 otherwise; `body.b64` observes base64 always.
- Resource files are bytes on both sides of the pipeline; a writer never
  re-encodes from a string.
- A viewer that reads bundles without the contracts package keeps its own
  three-arm reader. That is the one exception to "one decoder", accepted for a
  browser core that validates nothing; it reads the same arms and the same
  layout.
