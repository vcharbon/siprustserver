/**
 * The wire arms shared by a capture's message and a run's recorded line, and
 * the one decoder over them: whichever arm a document wrote, `datagramOf` is
 * the bytes that crossed the socket, and `payloadOfBytes` picks the arm by
 * those bytes alone — the same choice the Rust extractor makes.
 */
import { describe, expect, it } from "vitest"
import { Wire } from "../src/index.js"

const HEAD =
  "INFO sip:b@h SIP/2.0\r\nCSeq: 2 INFO\r\nContent-Type: application/vnd.example.blob\r\nContent-Length: 7\r\n\r\n"
/** Seven bytes, four of them never valid UTF-8 (`ff fe 80 00` after `00 01 02`). */
const BLOB = Uint8Array.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00])
const BLOB_B64 = "AAEC//6AAA=="

const utf8 = new TextEncoder()
const concat = (...parts: ReadonlyArray<Uint8Array>): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let at = 0
  for (const p of parts) {
    out.set(p, at)
    at += p.length
  }
  return out
}

describe("payloadOfBytes", () => {
  it("writes a whole-UTF-8 datagram as `raw`", () => {
    const bytes = utf8.encode("INVITE sip:é@h SIP/2.0\r\n\r\n")
    expect(Wire.payloadOfBytes(bytes)).toEqual({ _tag: "text", raw: "INVITE sip:é@h SIP/2.0\r\n\r\n" })
  })

  it("splits a UTF-8 head from a body that is not, as `head` + `body_b64`", () => {
    const bytes = concat(utf8.encode(HEAD), BLOB)
    expect(Wire.payloadOfBytes(bytes)).toEqual({ _tag: "head-body", head: HEAD, body_b64: BLOB_B64 })
  })

  it("writes a datagram whose head is not UTF-8 as `raw_b64`", () => {
    const bytes = concat(Uint8Array.from([0xff, 0xfe]), utf8.encode("INVITE sip:x SIP/2.0\r\n\r\n"))
    const payload = Wire.payloadOfBytes(bytes)
    expect(payload._tag).toBe("opaque")
    expect(Wire.datagramOf({ raw_b64: (payload as { raw_b64: string }).raw_b64 })).toEqual(bytes)
  })
})

describe("the head's end", () => {
  it("is the empty line, whichever terminator ends it — CRLF, LF or a bare CR — as the parser reads it", () => {
    for (const terminator of ["\r\n\r\n", "\n\n", "\n\r\n", "\r\r", "\r\n\r"]) {
      const head = `INFO sip:b@h SIP/2.0\r\nContent-Length: 7${terminator}`
      const bytes = concat(utf8.encode(head), BLOB)
      expect(Wire.payloadOfBytes(bytes), JSON.stringify(terminator)).toEqual({ _tag: "head-body", head, body_b64: BLOB_B64 })
      expect(Wire.headBodyOf({ raw: `${head}hello` })?.body, JSON.stringify(terminator)).toEqual(utf8.encode("hello"))
    }
    expect(Wire.headBodyOf({ raw: "INFO sip:b@h SIP/2.0\rContent-Length: 0\r" })?.body).toEqual(new Uint8Array(0))
  })
})

describe("datagramOf", () => {
  it("round-trips every arm to the same bytes", () => {
    for (const bytes of [
      utf8.encode("INVITE sip:é@h SIP/2.0\r\n\r\n"),
      concat(utf8.encode(HEAD), BLOB),
      concat(Uint8Array.from([0xff, 0xfe]), utf8.encode("INVITE sip:x SIP/2.0\r\n\r\n"))
    ]) {
      const payload = Wire.payloadOfBytes(bytes)
      const { _tag: _, ...arm } = payload
      expect(Wire.datagramOf(arm as Wire.Msg)).toEqual(bytes)
    }
  })

  it("decodes a `head` + `body_b64` line to the head's bytes followed by the body's", () => {
    expect(Wire.datagramOf({ head: HEAD, body_b64: BLOB_B64 })).toEqual(concat(utf8.encode(HEAD), BLOB))
  })
})

describe("headBodyOf", () => {
  it("hands back the head as text and the body as bytes, on the text and the split arm alike", () => {
    expect(Wire.headBodyOf({ head: HEAD, body_b64: BLOB_B64 })).toEqual({ head: HEAD, body: BLOB })
    const text = Wire.headBodyOf({ raw: "INFO sip:b@h SIP/2.0\r\nContent-Length: 5\r\n\r\nhello" })
    expect(text?.head).toBe("INFO sip:b@h SIP/2.0\r\nContent-Length: 5\r\n\r\n")
    expect(text?.body).toEqual(utf8.encode("hello"))
    expect(Wire.headBodyOf({ raw: "INFO sip:b@h SIP/2.0\r\n\r\n" })?.body).toEqual(new Uint8Array(0))
  })

  it("is undefined on an opaque datagram: it states no head to read", () => {
    expect(Wire.headBodyOf({ raw_b64: "//5JTkZPIHNpcDpiQGggU0lQLzIuMA0KDQo=" })).toBeUndefined()
  })
})

describe("textOf", () => {
  it("keeps the head intact and carries one character per body byte", () => {
    const text = Wire.textOf({ head: HEAD, body_b64: BLOB_B64 })
    expect(text.startsWith(HEAD)).toBe(true)
    expect(text.length).toBe(HEAD.length + BLOB.length)
    expect(text.charCodeAt(HEAD.length + 3)).toBe(0xff)
  })

  it("is the datagram itself on the text arm", () => {
    expect(Wire.textOf({ raw: "INVITE sip:é@h SIP/2.0\r\n\r\n" })).toBe("INVITE sip:é@h SIP/2.0\r\n\r\n")
  })
})

describe("utf8Of and latin1Of", () => {
  it("decode strictly: bytes that are not UTF-8 are no text", () => {
    expect(Wire.utf8Of(utf8.encode("héllo"))).toBe("héllo")
    expect(Wire.utf8Of(BLOB)).toBeUndefined()
    // One character per byte: the code units are the byte values.
    expect(Array.from(Wire.latin1Of(BLOB), (c) => c.charCodeAt(0))).toEqual([...BLOB])
  })
})
