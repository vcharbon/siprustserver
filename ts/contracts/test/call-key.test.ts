/** The key syntax: a Call-ID, and a qualifier the store hands back off. */
import { describe, expect, it } from "vitest"
import * as CallKey from "../src/call-key.js"

describe("a call key", () => {
  it("is the bare Call-ID where nothing qualifies it", () => {
    expect(CallKey.make("SDabc-1@x")).toBe("SDabc-1@x")
    expect(CallKey.callIdOf("SDabc-1@x")).toBe("SDabc-1@x")
  })

  it("carries its qualifier after a hash, and gives the Call-ID back without it", () => {
    const key = CallKey.make("SDabc-1@x", "ingress1")
    expect(key).toBe("SDabc-1@x#ingress1")
    expect(CallKey.callIdOf(key)).toBe("SDabc-1@x")
  })
})
