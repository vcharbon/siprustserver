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
})
