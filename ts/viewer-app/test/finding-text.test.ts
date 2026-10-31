import { describe, expect, it } from "vitest"
import { shortDifference, stepNamed } from "../src/finding-text.ts"

describe("finding text", () => {
  it("keeps what happened, without the address, the Call-ID and the rule's prose", () => {
    expect(
      shortDifference(
        "127.0.0.2:47195#ep0: Sent an SDP offer (callId t-A-1@pivot.invalid, CSeq 6) while its own offer on CSeq 5 was still unanswered — an offer is answered before the next one goes out (RFC 3264 §5)"
      )
    ).toBe("Sent an SDP offer while its own offer on CSeq 5 was still unanswered")
    expect(shortDifference("127.0.0.1:5080: 200 OK INVITE missing Supported: header — RFC 3261 §20.37"))
      .toBe("200 OK INVITE missing Supported: header")
  })

  it("leaves a detail with no address and no dash as it is", () => {
    expect(shortDifference("payload type 18 rebound")).toBe("payload type 18 rebound")
  })

  it("reads the step a statement names", () => {
    expect(stepNamed("rfc_violations: no-new-offer-while-offer-pending at s54 by uas1")).toBe("s54")
    expect(stepNamed("relayed: x originated by uac1 at s9; the reference relays it too")).toBe("s9")
    expect(stepNamed("nothing here")).toBeUndefined()
    expect(stepNamed(undefined)).toBeUndefined()
  })
})
