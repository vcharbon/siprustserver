/**
 * Values read for comparison: a media type apart from its boundary, Reason
 * (RFC 3326), Timestamp, CSeq and RAck values, and a session description's
 * origin line (RFC 4566 §5.2).
 */
import { describe, expect, it } from "vitest"
import {
  cseqMethodOf,
  descriptorForm,
  mediaTypeApartFromBoundary,
  rackParts,
  reasonForm,
  reasonKey,
  sessIdAlone,
  sessIdOf,
  timestampOf,
  topTypeRenderedApplication
} from "../src/value-forms.js"

describe("a media type (RFC 2046 §5.1.1)", () => {
  it("reads the type lowered and every parameter but the boundary, names lowered and values unquoted, sorted", () => {
    expect(mediaTypeApartFromBoundary('Multipart/Mixed; Boundary="unique-boundary-1"; type=application/sdp; Charset="UTF-8"'))
      .toEqual({ type: "multipart/mixed", params: "charset=UTF-8;type=application/sdp" })
  })

  it("reads the same form whatever boundary each side chose", () => {
    expect(mediaTypeApartFromBoundary("multipart/mixed;boundary=a")).toEqual(mediaTypeApartFromBoundary("multipart/mixed;boundary=b"))
  })
})

describe("media ranges rendered under another top type", () => {
  it("holds application ranges replaced one for one by another top type with the same subtypes", () => {
    expect(topTypeRenderedApplication(["application/sdp"], ["message/sdp"])).toBe(true)
  })

  it("never holds ranges that differ in count or subtype, or that stay application", () => {
    expect(topTypeRenderedApplication([], [])).toBe(false)
    expect(topTypeRenderedApplication(["application/sdp"], ["message/sdp", "message/sipfrag"])).toBe(false)
    expect(topTypeRenderedApplication(["application/sdp"], ["message/sipfrag"])).toBe(false)
    expect(topTypeRenderedApplication(["application/sdp"], ["application/sdp"])).toBe(false)
  })
})

describe("a Reason item (RFC 3326)", () => {
  it("reads the protocol lowered and the parameters sorted, apart from spacing and quoting", () => {
    expect(reasonForm('Q.850 ; text = "Normal call clearing" ; cause = 16')).toBe("q.850;cause=16;text=Normal call clearing")
    expect(reasonForm("SIP;cause=200")).toBe(reasonForm("sip ; cause=200"))
  })

  it("keys an item by its protocol and cause, the text left out", () => {
    expect(reasonKey('SIP;cause=200;text="Call completed elsewhere"')).toBe("sip;200")
    expect(reasonKey("Q.850;text=x")).toBe("q.850;")
  })
})

describe("descriptor, Timestamp, CSeq and RAck values", () => {
  it("reads a body descriptor lowered, the spacing around `;` and `=` dropped", () => {
    expect(descriptorForm(" Session ; Handling = Required ")).toBe("session;handling=required")
  })

  it("reads a Timestamp's own value, without the delay an echo adds (RFC 3261 §20.38)", () => {
    expect(timestampOf("54 0.5")).toBe("54")
    expect(timestampOf(" 54 ")).toBe("54")
  })

  it("reads a CSeq's method, none where the value states none", () => {
    expect(cseqMethodOf("314159 INVITE")).toBe("INVITE")
    expect(cseqMethodOf("314159")).toBeUndefined()
  })

  it("reads an RAck's RSeq and method (RFC 3262 §7.2), nothing from a malformed value", () => {
    expect(rackParts("776656 1 INVITE")).toEqual({ rseq: "776656", method: "INVITE" })
    expect(rackParts("776656 INVITE")).toBeUndefined()
    expect(rackParts("x 1 INVITE")).toBeUndefined()
  })
})

describe("a session description's origin (RFC 4566 §5.2)", () => {
  it("reads the sess-id of an origin line", () => {
    expect(sessIdOf("o=jdoe 2890844526 2890842807 IN IP4 192.0.2.10")).toBe("2890844526")
  })

  it("holds two origin lines that differ in the sess-id alone", () => {
    const captured = "o=jdoe 2890844526 2890842807 IN IP4 192.0.2.10"
    expect(sessIdAlone(captured, "o=jdoe 3344556677 2890842807 IN IP4 192.0.2.10")).toBe(true)
    expect(sessIdAlone(captured, captured)).toBe(false)
    expect(sessIdAlone(captured, "o=jdoe 3344556677 2890842808 IN IP4 192.0.2.10")).toBe(false)
    expect(sessIdAlone("o=jdoe 2890844526 IN IP4 192.0.2.10", "o=jdoe 3344556677 IN IP4 192.0.2.10")).toBe(false)
  })
})
