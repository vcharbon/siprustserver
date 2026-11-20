/**
 * A refused value in one line: the key as its path in the file, an array
 * element by its index, then what is wrong with it; a key the schema does not
 * name is "not a key of" the file the caller names, "this file" where it
 * names none.
 */
import * as Schema from "effect/Schema"
import { describe, expect, it } from "vitest"
import { decoderOf, issueLine } from "../src/manifest-file.js"

const Bug = Schema.Struct({ id: Schema.String, note: Schema.optionalKey(Schema.String) })
const File = Schema.Struct({
  known_bugs: Schema.optionalKey(Schema.Array(Bug)),
  ports: Schema.optionalKey(Schema.Array(Schema.Number)),
  timeouts: Schema.optionalKey(Schema.Struct({ open_s: Schema.Number }))
})

const decode = Schema.decodeUnknownResult(File, { onExcessProperty: "error", errors: "first" })

/** The schema's message of the first issue `raw` raises. */
const messageOf = (raw: unknown): string => {
  const decoded = decode(raw)
  if (decoded._tag === "Success") throw new Error("decoded")
  return decoded.failure.message
}

describe("a refused value's line", () => {
  it("names a key of a table by its dotted path", () => {
    expect(issueLine(messageOf({ timeouts: { open_s: "x" } }))).toBe("timeouts.open_s: Expected number")
  })

  it("names an element of an array by its index, and a key of it after", () => {
    expect(issueLine(messageOf({ ports: [5060, "x"] }))).toBe("ports[1]: Expected number")
    expect(issueLine(messageOf({ known_bugs: [{ id: "a" }, { id: 7 }] }))).toBe("known_bugs[1].id: Expected string")
  })

  it("names a key the schema does not name as no key of the file named, else of this file", () => {
    const excess = messageOf({ known_bugs: [{ id: "a", colour: "blue" }] })
    expect(issueLine(excess)).toBe("known_bugs[0].colour: not a key of this file")
    expect(issueLine(excess, "lane.toml")).toBe("known_bugs[0].colour: not a key of lane.toml")
  })

  it("is what a manifest decoder refuses with", () => {
    expect(decoderOf(File)({ ports: ["x"] })).toMatchObject({ _tag: "Failure", failure: "ports[0]: Expected number" })
  })
})
