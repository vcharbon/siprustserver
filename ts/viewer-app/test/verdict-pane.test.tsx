/**
 * Every step a verdict names is a link to the ladders: the steps a script
 * ended before, and the step a declared failure was observed at.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { VerdictPane } from "../src/components/VerdictPane.tsx"

const html = renderToStaticMarkup(
  <VerdictPane
    verdict={{
      status: "ok-negative",
      lane: "fake",
      must_fail: [{
        failure: "unexpected-final",
        step: "s4",
        derived_from: "capture",
        observed: { failure: "unexpected-final", step: "s5", got: "486" }
      }],
      abandoned: { leg: "B", step: "s5", pending: ["s6", "s7"], closed: [] }
    }}
    onPickStep={() => {}}
  />
)

describe("verdict pane", () => {
  it("links each step a script ended before", () => {
    expect(html).toContain(">s6</button>")
    expect(html).toContain(">s7</button>")
  })

  it("links a declared failure's anchor and the step it was observed at", () => {
    expect(html).toContain(">s4</button>")
    expect(html.match(/>s5<\/button>/g)?.length).toBe(2)
  })
})

describe("rfc violations", () => {
  const violation = { rule: "no-new-offer-while-offer-pending", step: "s54", emitter: "uas1", gating: false }
  const rfc = {
    status: "audited",
    findings: [{
      rule: "no-new-offer-while-offer-pending",
      lane: "127.0.0.1:5060",
      detail: "127.0.0.1:5060: Sent an SDP offer (callId c-1@pivot.invalid, CSeq 6) while CSeq 5 was unanswered — an offer is answered first (RFC 3264 §5)",
      advisory: true,
      gating: false,
      charged: "127.0.0.1:5060",
      actor: "uas1",
      cancelled_by: "rfc_violations: no-new-offer-while-offer-pending at s54 by uas1"
    }]
  }
  const markup = renderToStaticMarkup(
    <VerdictPane verdict={{ status: "ok", lane: "fake", rfc_violations: [violation] }} rfc={rfc} onPickStep={() => {}} />
  )

  it("charges a scripted party's violation to the reference capture", () => {
    expect(markup).toContain(">ref capture</span>")
    expect(markup).toContain("in the reference capture; not gating")
  })

  it("says the audit's short difference on the row, the whole finding on demand", () => {
    expect(markup).toContain(">Sent an SDP offer while CSeq 5 was unanswered</span>")
    expect(markup).toContain("RFC 3264 §5")
    expect(markup).toContain("finding-expand")
  })
})
