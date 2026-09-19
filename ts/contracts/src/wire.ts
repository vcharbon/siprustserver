/**
 * The wire arms a document carries a datagram in, and the one decoder over
 * them (ADR-0035), mirroring `sip_message::payload::Payload`.
 *
 * A datagram is BYTES. On disk it is written in EXACTLY ONE of three arms,
 * chosen purely from the bytes: `raw` when the whole datagram is UTF-8,
 * `head` + `body_b64` when only the body is not, `raw_b64` when not even the
 * head is. A capture's message (`Flows.Msg`) and a run's recorded line
 * (`Bundle.RecordedMessage`) spread the same three field sets, and every
 * reader decodes them here: `datagramOf` is the bytes that crossed the socket,
 * `headBodyOf` the head as text beside the body as bytes, `textOf` a rendering
 * for start-line and header readers. Text is a rendering, never the stored
 * form.
 *
 * Browser-safe by construction: no `Buffer`, no latin1 `TextDecoder` (not
 * byte-transparent everywhere) — `atob`/`btoa`, `String.fromCharCode` and a
 * fatal UTF-8 `TextDecoder`.
 */
import * as Schema from "effect/Schema"

/** Whole datagram is valid UTF-8 — the common, diff-readable case. */
export const textArm = { raw: Schema.String } as const
/** Start line + headers + blank line as UTF-8, then a body as standard base64. */
export const headBodyArm = { head: Schema.String, body_b64: Schema.String } as const
/** Even the head is not UTF-8 — opaque, standard base64. */
export const opaqueArm = { raw_b64: Schema.String } as const

export const TextArm = Schema.Struct(textArm)
export const HeadBodyArm = Schema.Struct(headBodyArm)
export const OpaqueArm = Schema.Struct(opaqueArm)

/** Any value carrying a datagram in one of the three arms. */
export type Msg =
  | { readonly raw: string }
  | { readonly head: string; readonly body_b64: string }
  | { readonly raw_b64: string }

/** The arm a message carries, as a tagged value. */
export type Payload =
  | { readonly _tag: "text"; readonly raw: string }
  | { readonly _tag: "head-body"; readonly head: string; readonly body_b64: string }
  | { readonly _tag: "opaque"; readonly raw_b64: string }

export const payloadOf = (msg: Msg): Payload => {
  if ("raw" in msg) return { _tag: "text", raw: msg.raw }
  if ("head" in msg) return { _tag: "head-body", head: msg.head, body_b64: msg.body_b64 }
  return { _tag: "opaque", raw_b64: msg.raw_b64 }
}

/** The arm's fields alone, as a document writes them beside its other keys. */
export const armOf = (payload: Payload): Msg => {
  switch (payload._tag) {
    case "text":
      return { raw: payload.raw }
    case "head-body":
      return { head: payload.head, body_b64: payload.body_b64 }
    case "opaque":
      return { raw_b64: payload.raw_b64 }
  }
}

const utf8Encoder = new TextEncoder()
const utf8Strict = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true })

/** The strict UTF-8 reading of `bytes`, or undefined where they are none. */
export const utf8Of = (bytes: Uint8Array): string | undefined => {
  try {
    return utf8Strict.decode(bytes)
  } catch {
    return undefined
  }
}

/** One character per byte: the code units are the byte values. */
export const latin1Of = (bytes: Uint8Array): string => {
  let out = ""
  const CHUNK = 0x2000
  for (let at = 0; at < bytes.length; at += CHUNK) {
    out += String.fromCharCode(...bytes.subarray(at, at + CHUNK))
  }
  return out
}

/** The bytes a latin1 string names, one per character (code unit & 0xff). */
export const bytesOfLatin1 = (text: string): Uint8Array => {
  const out = new Uint8Array(text.length)
  for (let i = 0; i < text.length; i++) out[i] = text.charCodeAt(i) & 0xff
  return out
}

/** Standard base64 (RFC 4648 §4, padded) of `bytes`. */
export const base64Of = (bytes: Uint8Array): string => btoa(latin1Of(bytes))

/**
 * The bytes a standard base64 text encodes. The reader is tolerant where the
 * Rust decoder is strict: an unpadded text decodes here and is refused there,
 * and a writer emits padded text so the two never disagree on what they wrote.
 */
export const bytesOfBase64 = (text: string): Uint8Array => bytesOfLatin1(atob(text))

/** The bytes of a UTF-8 string. */
export const bytesOfUtf8 = (text: string): Uint8Array => utf8Encoder.encode(text)

/**
 * Where the head ends: the index just past the empty line that closes the
 * header block (RFC 3261 §7), CRLFCRLF or a bare LFLF; undefined where the
 * head is unterminated. Mirrors `sip_message::sniff::body`.
 */
const headEndOf = (bytes: Uint8Array): number | undefined => {
  let crlf: number | undefined
  let lf: number | undefined
  for (let i = 0; i + 1 < bytes.length; i++) {
    if (lf === undefined && bytes[i] === 0x0a && bytes[i + 1] === 0x0a) lf = i + 2
    if (
      crlf === undefined &&
      i + 3 < bytes.length &&
      bytes[i] === 0x0d &&
      bytes[i + 1] === 0x0a &&
      bytes[i + 2] === 0x0d &&
      bytes[i + 3] === 0x0a
    ) {
      crlf = i + 4
    }
    if (crlf !== undefined && lf !== undefined) break
  }
  if (crlf !== undefined && lf !== undefined) return Math.min(crlf, lf)
  return crlf ?? lf
}

/**
 * The arm these bytes take, chosen purely from the bytes: whole-UTF-8 ⇒
 * `text`; else a UTF-8 head with a body after it ⇒ `head-body`; else
 * `opaque`. The same choice the Rust extractor makes, so a document
 * re-emitted from its bytes keeps its encoding.
 */
export const payloadOfBytes = (bytes: Uint8Array): Payload => {
  const whole = utf8Of(bytes)
  if (whole !== undefined) return { _tag: "text", raw: whole }
  const headEnd = headEndOf(bytes)
  if (headEnd !== undefined && headEnd < bytes.length) {
    const head = utf8Of(bytes.subarray(0, headEnd))
    if (head !== undefined) {
      return { _tag: "head-body", head, body_b64: base64Of(bytes.subarray(headEnd)) }
    }
  }
  return { _tag: "opaque", raw_b64: base64Of(bytes) }
}

/** The datagram that crossed the socket, whichever arm carries it. */
export const datagramOf = (msg: Msg): Uint8Array => {
  const payload = payloadOf(msg)
  switch (payload._tag) {
    case "text":
      return bytesOfUtf8(payload.raw)
    case "head-body": {
      const head = bytesOfUtf8(payload.head)
      const body = bytesOfBase64(payload.body_b64)
      const out = new Uint8Array(head.length + body.length)
      out.set(head, 0)
      out.set(body, head.length)
      return out
    }
    case "opaque":
      return bytesOfBase64(payload.raw_b64)
  }
}

/**
 * The head as text and the body as bytes. On the text arm the head runs
 * through the empty line that closes the header block (the whole datagram
 * where none does) and the body is what follows; undefined on an opaque
 * datagram, which states no head to read.
 */
export const headBodyOf = (msg: Msg): { readonly head: string; readonly body: Uint8Array } | undefined => {
  const payload = payloadOf(msg)
  switch (payload._tag) {
    case "text": {
      const bytes = bytesOfUtf8(payload.raw)
      const headEnd = headEndOf(bytes) ?? bytes.length
      return { head: utf8Of(bytes.subarray(0, headEnd)) ?? payload.raw, body: bytes.slice(headEnd) }
    }
    case "head-body":
      return { head: payload.head, body: bytesOfBase64(payload.body_b64) }
    case "opaque":
      return undefined
  }
}

/**
 * The datagram RENDERED as text for a start-line or header reader: the head
 * intact, then one character per body byte. On an opaque datagram, one
 * character per byte throughout.
 */
export const textOf = (msg: Msg): string => {
  const payload = payloadOf(msg)
  switch (payload._tag) {
    case "text":
      return payload.raw
    case "head-body":
      return payload.head + latin1Of(bytesOfBase64(payload.body_b64))
    case "opaque":
      return latin1Of(bytesOfBase64(payload.raw_b64))
  }
}
