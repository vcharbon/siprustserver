/**
 * The attribute order of a session description, the twin of the Rust
 * `sip_message::canonical_form`'s order rules: the shared fixture holds the
 * two alike case by case. The twin only moves lines: line terminators, blank
 * lines and a direction the description does not state stay as written.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { attributeOrder } from "../src/sdpform.js"

const FIXTURE = path.join(import.meta.dirname, "../../../crates/sip-message/tests/fixtures/sdp-form-order.txt")

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

  it("states its cases", () => {
    expect(cases.length).toBeGreaterThanOrEqual(8)
  })

  for (const c of cases) {
    it(c.name, () => {
      expect(attributeOrder(c.description)).toBe(c.form)
      expect(attributeOrder(c.form)).toBe(c.form)
    })
  }
})

describe("attributeOrder moves lines and nothing else", () => {
  const SESSION = ["v=0", "o=- 10 20 IN IP4 192.0.2.1", "s=-", "c=IN IP4 192.0.2.1", "t=0 0"]

  it("keeps each line terminator where it was, by position", () => {
    const lf = [...SESSION, "m=audio 49170 RTP/AVP 8", "a=sendrecv", "a=rtpmap:8 PCMA/8000", ""].join("\n")
    expect(attributeOrder(lf)).toBe([...SESSION, "m=audio 49170 RTP/AVP 8", "a=rtpmap:8 PCMA/8000", "a=sendrecv", ""].join("\n"))
    const mixed = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\ns=-\r\nt=0 0\r\nm=audio 49170 RTP/AVP 8\r\na=sendrecv\na=rtpmap:8 PCMA/8000\r\n"
    expect(attributeOrder(mixed))
      .toBe("v=0\r\no=- 10 20 IN IP4 192.0.2.1\ns=-\r\nt=0 0\r\nm=audio 49170 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\na=sendrecv\r\n")
  })

  it("adds no direction and no final line terminator", () => {
    const bare = [...SESSION, "m=audio 49170 RTP/AVP 8", "a=ptime:20", "a=rtpmap:8 PCMA/8000"].join("\r\n")
    expect(attributeOrder(bare)).toBe([...SESSION, "m=audio 49170 RTP/AVP 8", "a=rtpmap:8 PCMA/8000", "a=ptime:20"].join("\r\n"))
  })

  it("keeps a blank line, a non-attribute line among the section's own", () => {
    const blank = [...SESSION, "m=audio 49170 RTP/AVP 8", "a=sendrecv", "", "a=rtpmap:8 PCMA/8000", ""].join("\r\n")
    expect(attributeOrder(blank)).toBe([...SESSION, "m=audio 49170 RTP/AVP 8", "", "a=rtpmap:8 PCMA/8000", "a=sendrecv", ""].join("\r\n"))
  })

  it("leaves a text that is no session description, or states no media section, as written", () => {
    expect(attributeOrder("<xml/>")).toBe("<xml/>")
    expect(attributeOrder("")).toBe("")
    const noMedia = [...SESSION, "a=sendrecv", "a=tool:x", ""].join("\r\n")
    expect(attributeOrder(noMedia)).toBe(noMedia)
  })
})
