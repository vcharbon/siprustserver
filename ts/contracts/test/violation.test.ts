/**
 * The rule vocabulary mirrors the validator's: every rule `rfc_rules` has a
 * body for, in its order, as the Rust side pins it in a fixture.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { RFC_RULES } from "../src/violation.js"

const FIXTURE = path.join(
  import.meta.dirname,
  "../../../crates/pivot-schema/tests/fixtures/rfc-rule-tokens.txt"
)

describe("the rfc_violations rule vocabulary", () => {
  it("is the validator's rule tokens, position for position", () => {
    const tokens = fs.readFileSync(FIXTURE, "utf8").split("\n").filter((l) => l.length > 0)
    expect([...RFC_RULES]).toEqual(tokens)
  })
})
