/**
 * The capture pane over a message whose body is bytes: the head draws as it
 * always did, the SDP part draws as text, the indata part draws as a hex dump
 * under its own Content-Type line, and the ladder states the wire's size.
 */
import { buildFlowsLayout, parseFlowsDocument } from "@sip/viewer-core/flows"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { FlowsPane } from "../src/components/FlowsPane.tsx"
import { headBodyDocument, INDATA_DUMP, INFO_DUMP } from "../../viewer-core/test/head-body.ts"

const read = parseFlowsDocument(headBodyDocument)
if (!read.ok) throw new Error(read.reason)
const layout = buildFlowsLayout(read.document, { title: "indata.pcap" })

const paneWith = (row: number): string =>
  renderToStaticMarkup(
    <FlowsPane layout={layout} selected={layout.rows[row]} onSelect={() => {}} />
  )

describe("the capture pane over a binary body", () => {
  it("draws the multipart INVITE's head, its SDP as text and its indata as hex", () => {
    const markup = paneWith(0)
    expect(markup).toContain('<span class="sip-header-name">Content-Type</span>')
    expect(markup).toContain("multipart/mixed;boundary=unique-boundary-1")
    expect(markup).toContain("m=audio 4000 RTP/AVP 8")
    expect(markup).toContain("--unique-boundary-1\nContent-Type: application/vnd.sample.indata\n")
    expect(markup).toContain(`<span class="sip-hex">${INDATA_DUMP}\n</span>`)
    expect(markup).toContain("--unique-boundary-1--")
    expect(markup).toContain("<span class=\"sip-meta-value\">binary, multipart/mixed</span>")
  })

  it("draws the INFO's indata as one hex dump", () => {
    const markup = paneWith(1)
    expect(markup).toContain(`<span class="sip-hex">${INFO_DUMP}\n</span>`)
    expect(markup).toContain("<span class=\"sip-meta-value\">309</span>")
  })
})
