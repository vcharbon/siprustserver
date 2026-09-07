import { describe, expect, it } from "vitest"
import { foldOf, items, parseNameAddr, parseUri, setDelta, topLevelSplit, uriIsSecure, valuesEqual } from "../src/fold.js"

describe("the fold table", () => {
  it("advertisements compare as sets, identity as single, unknown names ordered", () => {
    expect(foldOf("Allow")).toBe("set")
    expect(foldOf("supported")).toBe("set")
    expect(foldOf("k")).toBe("set") // compact Supported
    expect(foldOf("From")).toBe("single")
    expect(foldOf("Via")).toBe("ordered")
    expect(foldOf("X-Custom-Header")).toBe("ordered")
  })
})

describe("item flattening", () => {
  it("a repeated line and a comma-appended item state the same set (§7.3.1)", () => {
    expect(valuesEqual("Supported", ["a, b"], ["a", "b"])).toBe(true)
    expect(valuesEqual("Allow", ["INVITE, ACK"], ["ACK, INVITE"])).toBe(true)
  })

  it("a comma inside a quoted display or angle brackets never tears", () => {
    expect(topLevelSplit(`"Doe, John" <sip:j@h>, <sip:k@h>`, ",")).toHaveLength(2)
    expect(items("Contact", [`"Doe, John" <sip:j@h>`])).toHaveLength(1)
  })

  it("an empty item is punctuation, never a value", () => {
    expect(valuesEqual("Allow", ["A, B,"], ["A, B"])).toBe(true)
    expect(items("Allow", ["A, B,"])).toEqual(["A", "B"])
  })

  it("the credentials family is never split — its comma is data", () => {
    const value = `Digest realm="r", nonce="a,b"`
    expect(items("WWW-Authenticate", [value])).toEqual([value])
  })

  it("set item text is verbatim — a case-only difference stays a difference", () => {
    expect(valuesEqual("Supported", ["100REL"], ["100rel"])).toBe(false)
  })

  it("Privacy splits on its semicolon separator", () => {
    expect(valuesEqual("Privacy", ["id;user"], ["user;id"])).toBe(true)
  })
})

describe("set deltas", () => {
  it("states which items moved, sorted, and only for set-folded names", () => {
    expect(setDelta("Allow", ["INVITE, ACK"], ["ACK, BYE, CANCEL"])).toEqual({
      added: ["BYE", "CANCEL"],
      removed: ["INVITE"]
    })
    expect(setDelta("Via", ["a"], ["b"])).toBeUndefined()
  })
})

describe("name-addr equivalence", () => {
  it("layout differences that state the same address are equal", () => {
    expect(valuesEqual("From", [`"A" <sip:+331@h.fr;user=phone>;tag=1`], [`"A" <sip:+331@H.FR; user=phone>;tag=1`])).toBe(
      true
    )
  })

  it("a tag difference is a difference", () => {
    expect(valuesEqual("From", ["<sip:a@h>;tag=1"], ["<sip:a@h>;tag=2"])).toBe(false)
  })

  it("parses bare addr-spec with header params", () => {
    const parsed = parseNameAddr("sip:+331@h.fr;tag=x")
    expect(parsed?.uri.user).toBe("+331")
    expect(parsed?.params.get("tag")).toBe("x")
  })
})

describe("URI reads", () => {
  it("tel and sip forms, hosts, ports, security", () => {
    expect(parseUri("tel:+33123;phone-context=fr")?.user).toBe("+33123")
    const sip = parseUri("sips:alice@Example.COM:5061;transport=tls")
    expect(sip?.host).toBe("example.com")
    expect(sip?.port).toBe(5061)
    expect(sip !== undefined && uriIsSecure(sip)).toBe(true)
    const plain = parseUri("sip:bob@h")
    expect(plain !== undefined && uriIsSecure(plain)).toBe(false)
  })
})
