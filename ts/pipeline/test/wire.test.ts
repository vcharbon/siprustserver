/**
 * The header-value readers the tier model leans on: a From/To value as a
 * dialog identity.
 */
import { describe, expect, it } from "vitest"
import { identityNameAddr } from "../src/wire.js"

describe("a From/To value as a dialog identity", () => {
  it("drops the tag and keeps every other header parameter in order", () => {
    expect(identityNameAddr("\"A\" <sip:+15556000001@h;user=phone>;x-param=1;tag=t1;x=y")).toBe(
      "\"A\" <sip:+15556000001@h;user=phone>;x-param=1;x=y"
    )
  })

  it("reads a `<` and a `;tag=` inside a quoted display name as the name's own", () => {
    expect(identityNameAddr("\"a<b;tag=x\" <sip:u@h>;tag=t1;p=1")).toBe("\"a<b;tag=x\" <sip:u@h>;p=1")
  })

  it("keeps a quoted header parameter value whole, its `;` included", () => {
    expect(identityNameAddr("<sip:u@h>;tag=t1;note=\"a;tag=b\";p=1")).toBe("<sip:u@h>;note=\"a;tag=b\";p=1")
  })

  it("brackets a bare addr-spec so its parameters stay the header's own (RFC 3261 §20.10)", () => {
    expect(identityNameAddr("sip:u@h;user=phone;tag=t1")).toBe("<sip:u@h>;user=phone")
  })
})
