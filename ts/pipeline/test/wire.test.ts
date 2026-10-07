/**
 * The header-value readers the tier model leans on: a From/To value as a
 * dialog identity.
 */
import { describe, expect, it } from "vitest"
import { Wire } from "@sip/contracts"
import { bodyBytesOf, headersInOrder, identityNameAddr } from "../src/wire.js"

describe("a From/To value as a dialog identity", () => {
  it("drops the tag and keeps every other header parameter in order", () => {
    expect(identityNameAddr("\"A\" <sip:+15556000001@h;user=phone>;x-param=1;tag=t1;x=y")).toBe(
      "\"A\" <sip:+15556000001@h;user=phone>;x-param=1;x=y"
    )
  })

  it("reads a `<` and a `;tag=` inside a quoted display name as the name's own", () => {
    expect(identityNameAddr("\"a<b;tag=x\" <sip:u@h>;tag=t1;p=1")).toBe("\"a<b;tag=x\" <sip:u@h>;p=1")
  })

  it("keeps a quoted header parameter value whole, its `;` included", () => {
    expect(identityNameAddr("<sip:u@h>;tag=t1;note=\"a;tag=b\";p=1")).toBe("<sip:u@h>;note=\"a;tag=b\";p=1")
  })

  it("brackets a bare addr-spec so its parameters stay the header's own (RFC 3261 §20.10)", () => {
    expect(identityNameAddr("sip:u@h;user=phone;tag=t1")).toBe("<sip:u@h>;user=phone")
  })
})

describe("a datagram whose head is not UTF-8", () => {
  /**
   * A display name carrying a supplementary character as two UTF-8-encoded
   * surrogates (CESU-8) is not UTF-8, so the datagram rides the opaque arm.
   * Its head is still a header block and its body still follows the blank
   * line: the split is bytewise, the head read one character per byte.
   */
  const head =
    "SIP/2.0 180 Ringing\r\nFrom: \"Ann \xed\xa0\xbc\xed\xbc\xb9\" <sip:a@h>;tag=1\r\n" +
    "To: <sip:b@h>;tag=2\r\nCall-ID: c\r\nCSeq: 1 INVITE\r\nContent-Type: application/sdp\r\n" +
    "Content-Length: 4\r\n\r\n"
  const bytes = Uint8Array.from([...head, ..."v=0\n"].map((c) => c.charCodeAt(0)))
  const msg: Wire.Msg = { raw_b64: Wire.base64Of(bytes) }

  it("is the opaque arm", () => {
    expect(Wire.payloadOfBytes(bytes)._tag).toBe("opaque")
  })

  it("still states its headers, in wire order", () => {
    expect(headersInOrder(msg).map((h) => h.name)).toEqual([
      "From",
      "To",
      "Call-ID",
      "CSeq",
      "Content-Type",
      "Content-Length"
    ])
    expect(headersInOrder(msg)[0]!.value).toBe("\"Ann \xed\xa0\xbc\xed\xbc\xb9\" <sip:a@h>;tag=1")
  })

  it("still states its body, as the bytes after the blank line", () => {
    expect([...bodyBytesOf(msg)]).toEqual([..."v=0\n"].map((c) => c.charCodeAt(0)))
  })
})
