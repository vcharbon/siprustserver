/**
 * A JSON document's packed strings opened: base64 of gzip JSON reads as the
 * JSON, base64 of gzip text as the text, nested ones too; any other string,
 * and one that only looks packed, stays as it is.
 */
import { gzipSync } from "node:zlib"
import { describe, expect, it } from "vitest"
import { jsonDecoded } from "../src/json-decoded.ts"

const packed = (text: string): string => gzipSync(Buffer.from(text)).toString("base64")

describe("a JSON document's packed strings", () => {
  it("open into the JSON or text they hold, at any depth", async () => {
    const inner = packed(JSON.stringify({ deep: true }))
    expect(await jsonDecoded({
      context: packed(JSON.stringify({ id: "r1", nested: inner })),
      note: packed("plain text"),
      list: [packed("[1,2]"), 3, null]
    })).toEqual({
      context: { id: "r1", nested: { deep: true } },
      note: "plain text",
      list: [[1, 2], 3, null]
    })
  })

  it("leave any other string as it is, one that only looks packed included", async () => {
    expect(await jsonDecoded({ a: "not-gzip", b: "H4sIAAAA", c: "" })).toEqual({ a: "not-gzip", b: "H4sIAAAA", c: "" })
  })
})
