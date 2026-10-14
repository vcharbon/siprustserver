/**
 * The RFC audit pane names a finding the SUT relayed from another party as
 * that party's: badged relayed, with the party and the statement that
 * cancelled it.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { RfcAuditPane } from "../src/components/RfcAuditPane.tsx"

describe("rfc audit pane", () => {
  it("shows a relayed finding as caused by its origin", () => {
    const html = renderToStaticMarkup(
      <RfcAuditPane
        rfc={{
          status: "audited",
          findings: [
            {
              rule: "payload-type-mapping-stable",
              lane: "127.0.0.1:5060",
              detail: "payload type 18 rebound",
              advisory: false,
              gating: false,
              charged: "127.0.0.1:5080",
              cancelled_by: "relayed: payload-type-mapping-stable originated by uac1 at s9; the reference relays it too",
              caused_by: "uac1"
            }
          ]
        }}
      />
    )
    expect(html).toContain("rfc-badge-relayed")
    expect(html).toContain("caused by uac1")
    expect(html).toContain("originated by uac1 at s9; the reference relays it too")
  })

  const party = {
    rule: "no-200-after-cancel",
    lane: "127.0.0.1:5060",
    detail: "answered 200 after the CANCEL",
    advisory: false,
    gating: false,
    charged: "127.0.0.1:5060",
    actor: "uas1"
  }

  it("lists a scripted party's finding the reference shows as information, never as why the cell is red", () => {
    const html = renderToStaticMarkup(
      <RfcAuditPane
        rfc={{
          status: "audited",
          findings: [{ ...party, cancelled_by: "rfc_violations: no-200-after-cancel at s12 by uas1" }]
        }}
      />
    )
    expect(html).toContain("rfc-badge-party")
    expect(html).toContain("the reference shows it")
    expect(html).not.toContain("rfc-audit-gating")
    expect(html).not.toContain("rfc-audit-harness")
  })

  it("lists an unjudged scripted party's finding as information, with why it was not judged", () => {
    const html = renderToStaticMarkup(
      <RfcAuditPane
        rfc={{ status: "audited", findings: [{ ...party, unjudged: "census-uncovered: no census of 'c'" }] }}
      />
    )
    expect(html).toContain("rfc-badge-party")
    expect(html).toContain("unjudged: census-uncovered: no census of &#x27;c&#x27;")
    expect(html).not.toContain("rfc-audit-gating")
  })

  it("draws a source fault in a section of its own, apart from the SUT's gating findings", () => {
    const html = renderToStaticMarkup(
      <RfcAuditPane rfc={{ status: "audited", findings: [{ ...party, source_fault: true }] }} />
    )
    expect(html).toContain("rfc-audit-source")
    expect(html).toContain("rfc-badge-source")
    expect(html).toContain("source fault")
    expect(html).not.toContain("harness")
    expect(html).not.toContain("rfc-audit-gating")
  })
})
