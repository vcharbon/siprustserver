import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { CaptureLegend } from "../src/components/CaptureLegend.tsx"
import { FlowsLadder } from "../src/components/FlowsLadder.tsx"
import { Ladder } from "../src/components/Ladder.tsx"
import { CallLegend, LadderLegend } from "../src/components/Legend.tsx"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { captureSelection } from "../src/flows/selection.ts"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { buildRecordingLayout } from "../src/recording/layout.ts"
import { parseRecording } from "../src/recording/parse.ts"
import { document, expectRequest, expectResponse, send } from "./documents.ts"

const layout = buildScenarioLayout(document([
  send("s1", "A", "INVITE"),
  {
    id: "a1",
    op: "alt",
    branches: [
      { name: "answered", steps: [expectResponse("s2", "A", 200, "OK")] },
      { name: "cancelled", steps: [expectResponse("s3", "A", 487, "Request Terminated")] }
    ]
  },
  { id: "i1", op: "inject", action: "node-kill", target: "b2bua-1" }
]))

describe("Ladder marks", () => {
  const markup = renderToStaticMarkup(
    <Ladder
      layout={layout}
      marksById={{ s2: { tone: "failure", text: "✗ retransmit-count-mismatch", title: "declared: 1 · observed: 2" } }}
    />
  )

  it("bands the row the finding was made at and names it there", () => {
    expect(markup).toContain("fv-mark fv-mark-failure")
    expect(markup).toContain("✗ retransmit-count-mismatch")
    expect(markup).toContain("declared: 1 · observed: 2")
  })

  it("marks only that row", () => {
    expect(markup.match(/fv-mark-band/g)?.length ?? 0).toBe(1)
    expect(markup).toContain("fv-arrow-marked fv-marked-failure")
  })

  it("draws nothing where no finding was made", () => {
    expect(renderToStaticMarkup(<Ladder layout={layout} />)).not.toContain("fv-mark-band")
  })
})

describe("Ladder", () => {
  const markup = renderToStaticMarkup(<Ladder layout={layout} selectedId="s2" />)

  it("draws every event and every branch", () => {
    expect(markup).toContain("INVITE")
    expect(markup).toContain("200 OK")
    expect(markup).toContain("487 Request Terminated")
    expect(markup).toContain("answered")
    expect(markup).toContain("cancelled")
    expect(markup).toContain("node-kill")
  })

  it("heads each alternative with its own numbered section", () => {
    expect(markup).toContain("alt 1/2 — answered")
    expect(markup).toContain("alt 2/2 — cancelled")
    expect(markup).toContain("2 exclusive alternatives, exactly one of them runs")
  })

  it("separates the sections with an exclusive-or mark and closes the alt with a rejoin", () => {
    expect(markup).toContain("fv-alt-separator")
    expect(markup).toContain("— OR —")
    // Two branches, one boundary: the first section carries no separator.
    expect(markup.split("fv-alt-separator")).toHaveLength(2)
    expect(markup).toContain("fv-alt-rejoin")
    expect(markup).toContain("all alternatives rejoin here")
  })

  it("repeats the participants inside each section and states the branch-local time base", () => {
    expect(markup.split("fv-branch-lifeline").length - 1).toBe(2 * layout.lifelines.length)
    expect(markup).toContain("time restarts at the alt entry")
  })

  it("marks the selected arrow", () => {
    expect(markup).toContain("fv-arrow-selected")
  })

  it("sizes the canvas from the layout", () => {
    expect(markup).toContain(`width="${layout.width}"`)
    expect(markup).toContain(`height="${layout.height}"`)
  })

  it("beads every arrow with its dialog token pair instead of real tags", () => {
    expect(markup).toContain("fv-dialog fv-dialog-from")
    expect(markup).toContain("fromA1")
    expect(markup).toContain("toA1")
  })

  it("marks the phase before a dialog is established with a hollow bead", () => {
    expect(markup).toContain("fv-dialog-absent")
    expect(markup).toContain("no To-tag yet")
  })

  it("beads a branch arrow like any other: full width leaves room for the split pair", () => {
    expect(markup).not.toContain("fv-dialog-pair")
    // One bead on each branch's response; the INVITE has no To-tag yet.
    expect(markup.split(">toA1<")).toHaveLength(3)
  })
})

describe("forked and multi-call ladders", () => {
  const forked = buildScenarioLayout(document([
    send("s1", "A", "INVITE"),
    expectRequest("s2", "B", "INVITE"),
    send("s3", "B", "183", { early: "f1", msg: { status: 183, "cseq-method": "INVITE" } }),
    send("s4", "B", "180", { early: "f2", msg: { status: 180, "cseq-method": "INVITE" } })
  ]))
  const forkedMarkup = renderToStaticMarkup(<Ladder layout={forked} />)

  it("gives each early dialog of a forked leg its own colour class", () => {
    expect(forkedMarkup).toContain("fv-fork-1")
    expect(forkedMarkup).toContain("fv-fork-2")
    expect(forkedMarkup).toContain("toB2")
  })

  it("holds the from-token constant across the fork, so the pair reads the fork", () => {
    expect(forkedMarkup.match(/>fromB1</g)?.length).toBe(3)
    expect(forkedMarkup).toContain(">toB1<")
    expect(forkedMarkup).toContain(">toB2<")
  })

  it("leaves a single-call ladder untinted", () => {
    expect(forkedMarkup).not.toContain("fv-tinted")
  })

  it("names the forked dialogs in the legend", () => {
    const legend = renderToStaticMarkup(<LadderLegend layout={forked} open />)
    expect(legend).toContain("forked early dialogs")
    expect(legend).toContain("early f2")
    // The chip states the whole pair, so the strip reads the fork the ladder shows.
    expect(legend).toContain("fromB1 · toB2 = early f2")
    expect(legend).toContain("same from, different to")
  })

  it("draws no call legend where a document holds one call", () => {
    expect(renderToStaticMarkup(<CallLegend layout={forked} />)).toBe("")
  })
})

describe("recording ladder beads", () => {
  const wire = (start: string, cseq: string, toTag?: string): string =>
    `${start}\r\nFrom: <sip:a@x>;tag=abc\r\nTo: <sip:b@x>${toTag === undefined ? "" : `;tag=${toTag}`}\r\nCSeq: ${cseq}\r\n\r\n`
  const recording = parseRecording({
    A: [
      JSON.stringify({ seq: 1, dir: "out", at_us: 0, step: "s1", raw: wire("INVITE sip:x SIP/2.0", "1 INVITE") }),
      JSON.stringify({ seq: 2, dir: "in", at_us: 1000, raw: wire("SIP/2.0 200 OK", "1 INVITE", "t1") }),
      JSON.stringify({ seq: 3, dir: "in", at_us: 2000, raw: wire("SIP/2.0 481 Call/Transaction Does Not Exist", "1 CANCEL", "t2") })
    ].join("\n")
  })
  const markup = renderToStaticMarkup(
    <Ladder layout={buildRecordingLayout(recording, { document: document([send("s1", "A", "INVITE")]) })} />
  )

  it("beads a second answering tag as a second to-token in its own fork colour", () => {
    expect(markup).toContain(">toA1<")
    expect(markup).toContain(">toA2<")
    expect(markup).toContain("fv-fork-1")
    expect(markup).toContain("fv-fork-2")
    expect(markup).toContain("answering tag t2")
  })
})

describe("FlowsLadder", () => {
  const captured = (callId: string, tsUs: number, src: string, dst: string) => ({
    call_id: callId,
    msgs: [{
      ts_us: tsUs,
      src,
      dst,
      retx: false,
      raw: "INVITE sip:x SIP/2.0\r\n\r\n",
      summary: { kind: "request", method: "INVITE", cseq: { seq: 1, method: "INVITE" } }
    }]
  })

  const read = parseFlowsDocument({
    schema: 5,
    legs: [captured("c", 10, "1.1.1.1:5060", "2.2.2.2:5060")],
    groups: []
  })

  it("draws one arrow per captured datagram, between its sockets", () => {
    if (!read.ok) throw new Error(read.reason)
    const markup = renderToStaticMarkup(<FlowsLadder layout={buildFlowsLayout(read.document)} />)
    expect(markup).toContain("1.1.1.1:5060")
    expect(markup).toContain("INVITE")
    expect(markup).toContain("leg 0/0")
  })

  const triage = parseFlowsDocument({
    schema: 5,
    legs: [
      captured("ingress", 10, "1.1.1.1:5060", "2.2.2.2:5060"),
      captured("SDaaa", 20, "2.2.2.2:5060", "3.3.3.3:5060"),
      captured("1-SDaaa", 30, "3.3.3.3:5060", "2.2.2.2:5060")
    ],
    groups: [{ legs: [0, 1, 2], evidence: [{ kind: "derived_call_id", legs: [1, 2], prefix: "1-" }] }]
  })
  if (!triage.ok) throw new Error(triage.reason)
  const triageLayout = buildFlowsLayout(triage.document, {
    selection: captureSelection(document([
      send("s1", "A", "INVITE", { observed: { at_us: 0, leg: 1, msg: 0 } }),
      send("s2", "B", "INVITE", { observed: { at_us: 5, leg: 2, msg: 0 } })
    ]))
  })
  const triageMarkup = renderToStaticMarkup(<FlowsLadder layout={triageLayout} />)

  it("draws a selected leg full and a discarded leg pale, both still clickable", () => {
    expect(triageMarkup).toContain("fv-picked")
    expect(triageMarkup).toContain("fv-pale")
    expect(triageMarkup.split("fv-arrow-hit")).toHaveLength(4)
    const discarded = triageMarkup.slice(triageMarkup.indexOf("leg 0/0") - 600, triageMarkup.indexOf("leg 0/0"))
    expect(discarded).toContain("fv-pale")
    expect(discarded).not.toContain("fv-picked")
  })

  it("gives a derived Call-ID its base's hue and its own shade", () => {
    const base = triageLayout.legs[1]!
    const derived = triageLayout.legs[2]!
    expect(triageMarkup).toContain(`fv-cid-${base.hueSlot} fv-shade-${base.shade} fv-picked`)
    expect(triageMarkup).toContain(`fv-cid-${derived.hueSlot} fv-shade-${derived.shade} fv-picked`)
    expect(derived.hueSlot).toBe(base.hueSlot)
  })

  it("heads each leg with its truncated Call-ID and a selected marker", () => {
    expect(triageMarkup).toContain("● leg 1 · SDaaa")
    expect(triageMarkup).toContain("○ leg 0 · ingress")
    expect(triageMarkup).toContain("1 msg +0ms")
  })

  it("keys the capture legend by Call-ID family, with each leg's verdict", () => {
    const legend = renderToStaticMarkup(<CaptureLegend layout={triageLayout} />)
    expect(legend).toContain("SDaaa")
    expect(legend).toContain("● selected")
    expect(legend).toContain("○ discarded")
    expect(legend).toContain("derived from leg 1")
  })
})
