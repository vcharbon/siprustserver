/**
 * A finding is one line: rule · chip · party · step · the short difference,
 * with the whole finding as the row's tooltip and behind its expander.
 */
import { buildRecordingLayout, parseRecording } from "@sip/viewer-core"
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
          offending_message: { leg: "B", seq: 3 },
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
    expect(html).toContain("ref capture")
  })

  it("keeps the whole finding on demand: the tooltip and an expander", () => {
    expect(html).toContain("RFC 3264 §5")
    expect(html).toContain("message B #3")
    expect(html).toContain("finding-expand")
    expect(html).not.toContain("finding-full")
  })

  it("numbers a finding's message as the ladder does, and says when the audit names none", () => {
    const recording = parseRecording({
      A: JSON.stringify({ seq: 1, dir: "out", at_us: 7, raw: "INVITE sip:b SIP/2.0\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n" }),
      B: [1, 2, 3].map((seq) =>
        JSON.stringify({ seq, dir: "in", at_us: 7, raw: `OPTIONS sip:c SIP/2.0\r\nCall-ID: b\r\nCSeq: ${seq} OPTIONS\r\n\r\n` })
      ).join("\n")
    })
    const layout = buildRecordingLayout(recording, {})
    const finding = { rule: "r", detail: "d", advisory: true, gating: false, offending: 54 }
    const on = renderToStaticMarkup(
      <RfcAuditPane rfc={{ status: "audited", findings: [{ ...finding, offending_message: { leg: "B", seq: 3 } }] }} layout={layout} />
    )
    const row = layout.events.find((e) => e.kind === "message" && e.legId === "B" && (e.source as { seq: number }).seq === 3)!.row
    expect(on).toContain(`message m${row}`)
    expect(on).not.toContain("m54")
    const none = renderToStaticMarkup(<RfcAuditPane rfc={{ status: "audited", findings: [finding] }} layout={layout} />)
    expect(none).toContain("no message pointer in this run&#x27;s audit; rerun")
  })

  it("heads the section with a noun and a count, no sentence", () => {
    expect(html).toContain("rfc audit · not gating <span class=\"count\">1</span>")
  })
})
