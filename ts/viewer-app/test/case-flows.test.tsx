/**
 * The case source pane over a document read a window of legs at a time: which
 * window it draws, what it says it drew, and that a leg drawn out of a window
 * keeps the index the document gives it.
 */
import { parseFlowsDocument } from "@sip/viewer-core/flows"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { PivotV3 } from "@sip/viewer-core"
import type { CaseSource } from "../src/api.ts"
import { caseFlowsView, caseWindow } from "../src/case-flows.ts"
import { SourceLadder } from "../src/components/SourcePane.tsx"

const legOf = (index: number) => ({
  call_id: `call-${index}@10.0.0.1`,
  final_status: 200,
  msgs: [{
    ts_us: 1_000_000 + index,
    src: "10.0.0.1:5060",
    dst: "10.0.0.2:5060",
    raw: "INVITE sip:x SIP/2.0\r\n\r\n",
    summary: { kind: "request", method: "INVITE", uri: "sip:x@host", cseq: { seq: 1, method: "INVITE" } }
  }]
})

const window = (from: number, count: number) => ({
  from,
  legs: Array.from({ length: count }, (_, index) => legOf(from + index))
})

const source: CaseSource = {
  available: true,
  capture: "c.pcap",
  callGroups: [12],
  path: "/corpus/c.flows.json",
  document: { schema: 5, groups: [], legs: [] },
  legs: 30_100,
  window: window(20_000, 200),
  cited: [],
  detail: ""
}

/** The same source for a case whose legs 25000 and 25001 lie past the opening window. */
const spread: CaseSource = { ...source, cited: [window(25_000, 2)] }

describe("the window the pane draws", () => {
  it("is the one the source response opened on", () => {
    expect(caseWindow(source, undefined)).toEqual({ from: 20_000, count: 200 })
  })

  it("is where the reader paged to, once they moved it", () => {
    expect(caseWindow(source, 20_200)).toEqual({ from: 20_200, count: 200 })
  })

  it("asks for nothing of a source with no document", () => {
    expect(caseWindow({ ...source, available: false, legs: 0, window: { from: 0, legs: [] } }, undefined))
      .toEqual({ from: 0, count: 0 })
  })
})

describe("the document the pane assembles", () => {
  it("holds the first window's legs at the document's own indices", () => {
    const view = caseFlowsView(source, undefined, undefined)
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs.length).toBe(30_100)
    expect(read.document.legs[20_000]?.callId).toBe("call-20000@10.0.0.1")
    expect(read.document.legs[0]).toBeUndefined()
  })

  it("holds the case's legs past the opening window too, and says how many", () => {
    const view = caseFlowsView(spread, undefined, undefined)
    expect(view.window).toEqual({ from: 20_000, count: 200 })
    expect(view.beyond).toBe(2)
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[25_001]?.callId).toBe("call-25001@10.0.0.1")
    expect(read.document.legs[24_999]).toBeUndefined()
  })

  it("holds the window the reader paged to alone: the case's far legs are the opening's", () => {
    const view = caseFlowsView(spread, 20_200, window(20_200, 200))
    expect(view.beyond).toBe(0)
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[25_000]).toBeUndefined()
  })

  it("holds the legs of the window the reader paged to, and none of the last one", () => {
    const view = caseFlowsView(source, 20_200, window(20_200, 200))
    expect(view.window).toEqual({ from: 20_200, count: 200 })
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[20_200]?.callId).toBe("call-20200@10.0.0.1")
    expect(read.document.legs[20_000]).toBeUndefined()
  })

  it("keeps the window it drew on screen while the one paged to is in flight", () => {
    const view = caseFlowsView(source, 20_200, undefined)
    expect(view.window).toEqual({ from: 20_000, count: 200 })
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[20_000]?.callId).toBe("call-20000@10.0.0.1")
  })
})

/** A case citing two legs deep in the document, as a cut writes one. */
const pivot = {
  pivot_version: 3,
  case: { id: "c-case1" },
  flow: [
    { op: "send", id: "s1", observed: { leg: 20_001, msg: 0 } },
    { op: "expect", id: "s2", observed: { leg: 20_002, msg: 0 } }
  ]
} as unknown as PivotV3

describe("the window line and the legs-left-out warning", () => {
  /** The pane over the window `from`, drawn for a case citing 20001 and 20002. */
  const paneAt = (from: number): string =>
    renderToStaticMarkup(
      <SourceLadder
        source={source}
        pivot={pivot}
        flows={{
          ...caseFlowsView(source, from === source.window.from ? undefined : from, window(from, 200)),
          loading: false,
          error: undefined,
          onWindow: () => {}
        }}
      />
    )

  it("count in one base: a window holding the case's legs says nothing of them", () => {
    const markup = paneAt(20_000)
    expect(markup).toContain("legs 20000–20199 of 30100")
    expect(markup).not.toContain("outside the window drawn")
  })

  it("names them only once the line names a window past them", () => {
    const markup = paneAt(20_200)
    expect(markup).toContain("legs 20200–20399 of 30100")
    expect(markup).toContain("2 legs of this case lie outside the window drawn: 20001, 20002")
  })
})

describe("the pane over a window", () => {
  const markup = (from: number) =>
    renderToStaticMarkup(
      <SourceLadder
        source={source}
        pivot={undefined}
        flows={{
          ...caseFlowsView(source, from === 20_000 ? undefined : from, window(from, 200)),
          loading: false,
          error: undefined,
          onWindow: () => {}
        }}
      />
    )

  it("draws the window's legs under the indices the document gives them", () => {
    expect(markup(20_000)).toContain("leg 20000")
  })

  it("states which legs of the document it drew", () => {
    expect(markup(20_000)).toContain("legs 20000–20199 of 30100")
  })

  it("offers the windows either side, and says so once the reader paged", () => {
    expect(markup(20_000)).toContain("later legs")
    expect(markup(20_200)).toContain("legs 20200–20399 of 30100")
  })
})
