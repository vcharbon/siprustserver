/**
 * A finding is one line: rule · chip · party · step · the short difference,
 * with the whole finding as the row's tooltip and behind its expander.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { RfcAuditPane } from "../src/components/RfcAuditPane.tsx"

describe("finding rows", () => {
  const html = renderToStaticMarkup(
    <RfcAuditPane
      rfc={{
        status: "audited",
        findings: [{
          rule: "no-new-offer-while-offer-pending",
          lane: "127.0.0.1:5060",
          detail: "127.0.0.1:5060: Sent an SDP offer (callId c-1@pivot.invalid, CSeq 6) while CSeq 5 was unanswered — an offer is answered first (RFC 3264 §5)",
          advisory: true,
          gating: false,
          offending: 54,
          charged: "127.0.0.1:5060",
          actor: "uas1",
          cancelled_by: "rfc_violations: no-new-offer-while-offer-pending at s54 by uas1"
        }]
      }}
      onPickStep={() => {}}
    />
  )

  it("says the finding in one short line", () => {
    expect(html).toContain(">Sent an SDP offer while CSeq 5 was unanswered</span>")
    expect(html).toContain(">uas1</span>")
    expect(html).toContain(">s54</button>")
    expect(html).toContain("scripted · ref")
  })

  it("keeps the whole finding on demand: the tooltip and an expander", () => {
    expect(html).toContain("RFC 3264 §5")
    expect(html).toContain("message 54")
    expect(html).toContain("finding-expand")
    expect(html).not.toContain("finding-full")
  })

  it("heads the section with a noun and a count, no sentence", () => {
    expect(html).toContain("rfc audit · not gating <span class=\"count\">1</span>")
  })
})
