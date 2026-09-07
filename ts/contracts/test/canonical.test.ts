/**
 * The canonical formatter against the cases `pivot_schema::canonical`'s own unit
 * tests pin. Both halves must write the same bytes, so both halves are held to
 * the same examples.
 */
import { describe, expect, it } from "vitest"
import { format, formatDeclared, formatLine, formatText } from "../src/canonical.js"

describe("format", () => {
  it("sorts keys at every level and ends the document in one newline", () => {
    expect(format({ b: 1, a: { z: [1, 2], y: true } })).toBe(
      '{\n  "a": {\n    "y": true,\n    "z": [\n      1,\n      2\n    ]\n  },\n  "b": 1\n}\n'
    )
  })

  it("is idempotent", () => {
    const once = format({ b: [{ d: 4, c: 3 }], a: "x" })
    expect(formatText(once)).toBe(once)
  })

  it("keeps empty collections compact and strings escaped", () => {
    expect(format({ a: [], b: {} })).toBe('{\n  "a": [],\n  "b": {}\n}\n')
    expect(format('a"b\\c\nd')).toBe('"a\\"b\\\\c\\nd"\n')
  })

  it("leaves non-ASCII literal, so the Rust emitter agrees", () => {
    expect(format("café — ok")).toBe('"café — ok"\n')
  })

  it("drops an absent optional key rather than writing it as null", () => {
    expect(format({ a: 1, b: undefined })).toBe('{\n  "a": 1\n}\n')
  })

  it("refuses malformed input rather than passing it through", () => {
    expect(() => formatText('{"a": }')).toThrow()
  })
})

describe("formatLine", () => {
  it("holds the document key order on one line and ends without a newline", () => {
    expect(formatLine({ b: 1, a: { z: [1, 2], y: true } })).toBe('{"a":{"y":true,"z":[1,2]},"b":1}')
    expect(formatLine({ a: [], b: {} })).toBe('{"a":[],"b":{}}')
  })

  it("states the same keys, order and escaping as the document form", () => {
    const value = { b: 1, a: { z: [1, 2], y: true } }
    expect(formatText(formatLine(value))).toBe(format(value))
  })
})

describe("formatDeclared", () => {
  it("keeps the caller's key order — the e2e records are not sorted", () => {
    expect(formatDeclared({ b: 1, a: 2 })).toBe('{\n  "b": 1,\n  "a": 2\n}\n')
  })
})
