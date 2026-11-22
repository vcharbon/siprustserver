/**
 * The `sdp` fold against the stack's canonical form: every description of the
 * shared fixture the Rust `sip_message::canonical_form` is held to compares
 * equal to its form on every run, so a description a stack re-serializes
 * reads as the one it was written as.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { diffSdp, FLOOR, foldSdp, type SdpMask, VERBATIM } from "../src/sdpfold.js"

const FIXTURE = path.join(import.meta.dirname, "../../../crates/sip-message/tests/fixtures/sdp-form-order.txt")

const MASKS: ReadonlyArray<SdpMask> = [VERBATIM, FLOOR, { origin: true, connectionAddress: true, mediaPort: true }]

interface OrderCase {
  readonly name: string
  description: string
  form: string
}

/** The fixture's cases, every SDP line ended with CRLF; `#` lines and blank lines are no SDP line. */
const orderCases = (): ReadonlyArray<OrderCase> => {
  const cases: Array<OrderCase> = []
  let inForm = false
  for (const line of fs.readFileSync(FIXTURE, "utf8").split("\n")) {
    if (line === "" || line.startsWith("#")) continue
    if (line.startsWith("=== ")) {
      cases.push({ name: line.slice(4), description: "", form: "" })
      inForm = false
      continue
    }
    const current = cases[cases.length - 1]!
    if (line === "---") {
      inForm = true
      continue
    }
    if (inForm) current.form += `${line}\r\n`
    else current.description += `${line}\r\n`
  }
  return cases
}

describe("the shared order fixture", () => {
  const cases = orderCases()

  it("states its cases, each one a form that moves lines", () => {
    expect(cases.length).toBeGreaterThanOrEqual(8)
    expect(cases.filter((c) => c.description !== c.form).length).toBeGreaterThanOrEqual(8)
  })

  for (const c of cases) {
    it(c.name, () => {
      for (const mask of MASKS) {
        expect(diffSdp(mask, c.description, c.form)).toEqual([])
        expect(foldSdp(mask, c.description)).toBe(foldSdp(mask, c.form))
      }
    })
  }
})
