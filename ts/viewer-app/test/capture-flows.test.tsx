/**
 * The capture pane over a document read a window of legs at a time: which
 * window the page asks the server for, what it says it drew, and that the
 * ladder's coordinates stay the DOCUMENT's — a leg drawn out of a window keeps
 * the index every case and every URL cites it by.
 */
import { buildFlowsLayout, captureSelection, parseFlowsDocument, windowedFlows } from "@sip/viewer-core/flows"
import type { PivotV3 } from "@sip/viewer-core"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { captureFlowsView, captureKey, captureReads, captureWindow } from "../src/capture-flows.ts"
import { windowLine } from "../src/components/WindowLine.tsx"
import { FlowsPane } from "../src/components/FlowsPane.tsx"

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

const envelope = { schema: 5, groups: [], legs: [] }

describe("the window the page asks for", () => {
  it("is the whole of a capture that fits", () => {
    expect(captureWindow({ path: "/f.json", legs: 40 }, [], undefined)).toEqual({ from: 0, count: 40 })
  })

  it("opens on the legs the case cites", () => {
    expect(captureWindow({ path: "/f.json", legs: 30_100 }, [20_000], undefined).from).toBe(20_000)
  })

  it("opens where the reader moved it, whatever the case cites", () => {
    expect(captureWindow({ path: "/f.json", legs: 30_100 }, [20_000], 400).from).toBe(400)
  })

  it("asks for nothing where the capture has no document", () => {
    expect(captureWindow(undefined, [], undefined)).toEqual({ from: 0, count: 0 })
  })
})

describe("the reads the page asks for", () => {
  const flows = { path: "/f.json", legs: 30_100 }

  it("open on the case's legs: the window at them, then each run of legs past it", () => {
    expect(captureReads(flows, [20_003, 20_000, 20_500, 20_501, 25_000], undefined)).toEqual([
      { from: 20_000, count: 200 },
      { from: 20_500, count: 2 },
      { from: 25_000, count: 1 }
    ])
  })

  it("are the one window where the case's legs fit it", () => {
    expect(captureReads(flows, [20_000, 20_199], undefined)).toEqual([{ from: 20_000, count: 200 }])
  })

  it("are the window the reader moved to alone, whatever the case cites", () => {
    expect(captureReads(flows, [20_000, 25_000], 400)).toEqual([{ from: 400, count: 200 }])
  })
})

describe("what one capture's window and legs are read against", () => {
  const A = { path: "/a.flows.json", legs: 30_100 }
  const B = { path: "/b.flows.json", legs: 500 }
  const keyA = captureKey("a.pcap", A)
  const keyB = captureKey("b.pcap", B)
  const envelopeA = { key: keyA, value: { ...A, document: envelope } }
  const legsA = {
    key: keyA,
    value: [{ from: 20_000, legs: Array.from({ length: 200 }, (_, index) => legOf(20_000 + index)) }]
  }
  const at = { key: keyA, from: 20_000 }

  it("draws the case's legs past the window it opened on, and says how many", () => {
    const held = { key: keyA, value: [...legsA.value, { from: 25_000, legs: [legOf(25_000)] }] }
    const view = captureFlowsView(keyA, A, [20_001, 25_000], undefined, envelopeA, held)
    expect(view.window).toEqual({ from: 20_000, count: 200 })
    expect(view.beyond).toBe(1)
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[25_000]?.callId).toBe("call-25000@10.0.0.1")
    expect(read.document.legs[24_999]).toBeUndefined()
    expect(windowLine(view.window, A.legs, view.beyond)).toBe("legs 20000–20199 of 30100, and the case's 1 leg past them")
  })

  it("draws the window the reader opened on this capture, as far as its legs reach", () => {
    const view = captureFlowsView(keyA, A, [], at, envelopeA, legsA)
    expect(view.window).toEqual({ from: 20_000, count: 200 })
    const read = parseFlowsDocument(view.document)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[20_000]?.callId).toBe("call-20000@10.0.0.1")
  })

  it("opens the NEXT capture on its own first leg, not where the last one stood", () => {
    expect(captureFlowsView(keyB, B, [], at, envelopeA, legsA).window).toEqual({ from: 0, count: 200 })
  })

  it("draws nothing of the capture the held envelope and legs were read for", () => {
    expect(captureFlowsView(keyB, B, [], at, envelopeA, legsA).document).toBeUndefined()
  })

  it("draws nothing while the legs of a window it has no envelope for are held", () => {
    expect(captureFlowsView(keyA, A, [], at, undefined, legsA).document).toBeUndefined()
  })
})

describe("what the pane says it drew", () => {
  it("says nothing where the window is the whole document", () => {
    expect(windowLine({ from: 0, count: 40 }, 40)).toBeUndefined()
  })

  it("names the legs drawn and the document they are out of", () => {
    expect(windowLine({ from: 20_000, count: 200 }, 30_100)).toBe("legs 20000–20199 of 30100")
  })
})

describe("the study pane's window line and its legs-left-out warning", () => {
  const selection = captureSelection(
    {
      flow: [
        { op: "send", id: "s1", observed: { leg: 20_001, msg: 0 } },
        { op: "expect", id: "s2", observed: { leg: 20_002, msg: 0 } }
      ]
    } as unknown as PivotV3
  )
  /** The capture pane over the window `from`, for a case citing 20001 and 20002. */
  const paneAt = (from: number): string => {
    const legs = Array.from({ length: 200 }, (_, at) => legOf(from + at))
    const read = parseFlowsDocument(windowedFlows(envelope, 30_100, from, legs))
    if (!read.ok) throw new Error(read.reason)
    return renderToStaticMarkup(
      <FlowsPane
        layout={buildFlowsLayout(read.document, { selection, title: "c.pcap" })}
        selected={undefined}
        onSelect={() => {}}
        window={{ from, count: 200, total: 30_100, onWindow: () => {} }}
      />
    )
  }

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

describe("the ladder of a window", () => {
  const read = parseFlowsDocument(
    windowedFlows(envelope, 30_100, 20_000, [legOf(20_000), legOf(20_001)])
  )
  if (!read.ok) throw new Error(read.reason)
  const layout = buildFlowsLayout(read.document, { title: "c.pcap" })
  const markup = renderToStaticMarkup(
    <FlowsPane
      layout={layout}
      selected={undefined}
      onSelect={() => {}}
      window={{ from: 20_000, count: 200, total: 30_100, onWindow: () => {} }}
    />
  )

  it("draws the window's legs under the indices the document gives them", () => {
    expect(markup).toContain("leg 20000/0")
    expect(markup).toContain("leg 20001/0")
  })

  it("states which legs of the document it drew", () => {
    expect(markup).toContain("legs 20000–20199 of 30100")
  })

  it("offers the windows either side of the one drawn", () => {
    expect(markup).toContain("earlier legs")
    expect(markup).toContain("later legs")
  })
})
