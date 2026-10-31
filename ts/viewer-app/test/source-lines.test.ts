/** A source's lines as the case drawer numbers them. */
import { describe, expect, it } from "vitest"
import { sourceLines } from "../src/source-lines.ts"

describe("sourceLines", () => {
  it("reads LF and CRLF alike, a final line break ending the last line", () => {
    expect(sourceLines("a\nb")).toEqual(["a", "b"])
    expect(sourceLines("a\nb\n")).toEqual(["a", "b"])
    expect(sourceLines("a\r\nb\r\n")).toEqual(["a", "b"])
  })

  it("keeps blank lines inside the text, and an empty text as one empty line", () => {
    expect(sourceLines("a\n\nb\n")).toEqual(["a", "", "b"])
    expect(sourceLines("")).toEqual([""])
  })
})
