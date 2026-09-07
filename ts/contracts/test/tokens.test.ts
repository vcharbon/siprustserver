/**
 * The string tokens, held to the same examples `pivot_schema`'s own unit tests
 * hold the Rust enums to. A token that round-trips in one language and not the
 * other is a document that changes meaning when it crosses the seam.
 */
import { describe, expect, it } from "vitest"
import * as Schema from "effect/Schema"
import {
  accessorPresentIn,
  accessorTarget,
  accessorToken,
  Anchor,
  anchorStep,
  anchorToken,
  Cause,
  causeToken,
  laneVerdictIsOk,
  laneVerdictToken,
  parseAccessor,
  parseAnchor,
  parseCause,
  parseLaneVerdict,
  parsePosition,
  positionToken,
  scanAccessors
} from "../src/tokens.js"

const decodeAnchor = Schema.decodeUnknownSync(Anchor)
const decodeCause = Schema.decodeUnknownSync(Cause)

describe("a lane verdict", () => {
  it("round-trips through its token", () => {
    for (const token of ["ok", "blocked:number-unclassified", "blocked:claim-ambiguous"]) {
      expect(laneVerdictToken(parseLaneVerdict(token))).toBe(token)
    }
    expect(laneVerdictIsOk("ok")).toBe(true)
    expect(laneVerdictIsOk("blocked:x")).toBe(false)
  })

  it("refuses a reasonless or unknown verdict", () => {
    expect(() => parseLaneVerdict("blocked:")).toThrow()
    expect(() => parseLaneVerdict("skipped")).toThrow()
  })
})

describe("an attempt cause", () => {
  it("round-trips every member through its token", () => {
    for (const token of [
      "no-answer",
      "busy",
      "transaction-timeout",
      "closed:bye",
      "redirect:302",
      "external:486",
      "external:603"
    ]) {
      expect(causeToken(parseCause(token))).toBe(token)
      expect(decodeCause(token)).toBe(token)
    }
  })

  it("refuses a status outside its class, and a closer cited by the wrong name", () => {
    for (const token of ["redirect:404", "external:302", "external:700", "redirect:x", "abandoned", "closed:cancel"]) {
      expect(() => parseCause(token)).toThrow()
    }
    expect(() => decodeCause("redirect:x")).toThrow()
  })
})

describe("a delay anchor", () => {
  it("round-trips through its token", () => {
    for (const token of ["trigger", "step:s1", "step:invite-out"]) {
      expect(anchorToken(parseAnchor(token))).toBe(token)
      expect(decodeAnchor(token)).toBe(token)
    }
    expect(anchorStep("step:s1")).toBe("s1")
    expect(anchorStep("trigger")).toBeUndefined()
  })

  it("refuses a malformed anchor", () => {
    for (const token of ["step:", "start", "", "s1"]) {
      expect(() => decodeAnchor(token)).toThrow()
    }
  })
})

describe("an accessor", () => {
  it("round-trips every documented form", () => {
    for (const token of [
      "${leg:B.call-id}",
      "${leg:B.local-tag}",
      "${leg:B.remote-tag}",
      "${leg:B.remote-target}",
      "${leg:B.route-set}",
      "${leg:B.cseq.local}",
      "${leg:B.cseq.remote}",
      "${leg:B.rseq}",
      "${early:f1.tag}",
      "${early:f2.rseq}",
      "${step:s7.header.To}",
      "${step:s7.cseq}",
      "${step:s7.rseq}",
      "${step:s7.status}",
      "${step:a1.branch}",
      "${num:caller:private}",
      "${num:called-0-1:trunk-composed}"
    ]) {
      expect(accessorToken(parseAccessor(token))).toBe(token)
    }
  })

  it("names an identity and the form to dial it in", () => {
    const accessor = parseAccessor("${num:transferee:e164}")
    expect(accessor).toEqual({ _tag: "num", name: "transferee", form: "e164" })
    expect(accessorTarget(accessor)).toBe("transferee")
  })

  it("refuses a number accessor without both halves", () => {
    for (const token of ["${num:transferee}", "${num::e164}", "${num:transferee:}", "${num:}"]) {
      expect(() => parseAccessor(token)).toThrow()
    }
  })

  it("refuses a field outside the vocabulary, by name", () => {
    for (const token of [
      "${leg:B.local-target}",
      "${step:s7.body}",
      "${dialog:B.call-id}",
      "${leg:B}",
      "${leg:.call-id}",
      "${step:s7.header.}",
      "leg:B.call-id",
      "${early:f1.local-tag}",
      "${early:f1}",
      "${early:.tag}"
    ]) {
      expect(() => parseAccessor(token)).toThrow()
    }
  })

  it("finds every accessor embedded in a header value", () => {
    const value = "<sip:x@h>;?X-Dialog=${leg:B.call-id}%3Bto-tag%3D${leg:B.remote-tag}"
    const found = scanAccessors(value).map((a) => accessorToken(a as never))
    expect(found).toEqual(["${leg:B.call-id}", "${leg:B.remote-tag}"])
    expect(scanAccessors("no accessors here")).toEqual([])
    expect(accessorPresentIn("no accessors here")).toBe(false)
  })

  it("treats an unterminated accessor as a refusal, not literal text", () => {
    const found = scanAccessors("<sip:x@h>;tag=${leg:B.remote-tag")
    expect(found.length).toBe(1)
    expect(found[0]).toBeInstanceOf(Error)
  })
})

describe("a position token", () => {
  it("round-trips bare and call-qualified", () => {
    for (const token of ["caller", "called[0][0]", "called[1][2]", "c2.caller", "c2.called[0][1]"]) {
      expect(positionToken(parsePosition(token))).toBe(token)
    }
  })

  it("refuses a malformed position", () => {
    for (const token of ["called", "called[0]", "called[a][0]", "callee[0][0]", ""]) {
      expect(() => parsePosition(token)).toThrow()
    }
  })
})
