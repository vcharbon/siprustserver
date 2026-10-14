/**
 * The ladder delta of two runs of one call: ladders that differ only in their
 * titles, their time column and the CSeq base each run's endpoints minted
 * read as no delta; a missing ACK or another status is the first difference,
 * at the event it moves, and counts once.
 */
import { describe, expect, it } from "vitest"
import { ladderDelta, normaliseLadder } from "../src/ladder-delta.js"

interface Row {
  readonly t: string
  readonly from: string
  readonly to: string
  readonly message: string
  readonly detail: string
  readonly dialog: string
  readonly step: string
}

/** A replay ladder as the text renderer draws one: title, subtitle, blank, column header, one line per event. */
const ladder = (title: string, rows: ReadonlyArray<Row>): ReadonlyArray<string> => [
  title,
  `2 legs · ${rows.length} messages`,
  "",
  `ladder (${rows.length} events; m<idx> +t from -> to | message | detail | dialog | step):`,
  ...rows.map((row, at) =>
    `  m${at} ${row.t} ${row.from} -> ${row.to} | ${row.message} | ${row.detail} | ${row.dialog} | ${row.step}`
  )
]

/** One call's rows; the callee leg's CSeq base and every time are the run's own. */
const rows = (base: number, times: ReadonlyArray<string>, over: { readonly final?: string; readonly ack?: boolean } = {}) => {
  const final = over.final ?? "200 OK"
  const all: Array<Row> = [
    { t: times[0]!, from: "uac1", to: "SUT", message: "INVITE", detail: "CSeq 1", dialog: "fromA1", step: "s1" },
    { t: times[1]!, from: "SUT", to: "uas1", message: "INVITE", detail: `CSeq ${base}`, dialog: "fromB1", step: "s2" },
    {
      t: times[2]!,
      from: "uas1",
      to: "SUT",
      message: final,
      detail: `INVITE · CSeq ${base}`,
      dialog: "fromB1 · toB1",
      step: "s3"
    },
    {
      t: times[3]!,
      from: "SUT",
      to: "uac1",
      message: final,
      detail: "INVITE · CSeq 1",
      dialog: "fromA1 · toA1",
      step: "s4"
    },
    { t: times[4]!, from: "uac1", to: "SUT", message: "ACK", detail: "CSeq 1", dialog: "fromA1 · toA1", step: "s5" },
    { t: times[5]!, from: "SUT", to: "uas1", message: "ACK", detail: `CSeq ${base}`, dialog: "fromB1 · toB1", step: "s6" },
    { t: times[6]!, from: "SUT", to: "uac1", message: "OPTIONS", detail: "CSeq 77", dialog: "fromA1 · toA1", step: "A#1" },
    {
      t: times[7]!,
      from: "uac1",
      to: "SUT",
      message: "200 OK",
      detail: "OPTIONS · CSeq 77",
      dialog: "fromA1 · toA1",
      step: "A#2"
    },
    { t: times[8]!, from: "uac1", to: "SUT", message: "BYE", detail: "CSeq 2", dialog: "fromA1 · toA1", step: "s7" },
    { t: times[9]!, from: "SUT", to: "uas1", message: "BYE", detail: `CSeq ${base + 1}`, dialog: "fromB1 · toB1", step: "s8" }
  ]
  return over.ack === false ? all.filter((row) => !(row.message === "ACK" && row.from === "SUT")) : all
}

const FAKE = ["+0.000000", "+0.002000", "+0.347000", "+0.349000", "+0.467000", "+0.469000", "+30.349000", "+30.349000", "+40.000000", "+40.002000"]
const REAL = ["+0.000000", "+0.017807", "+0.391220", "+0.405418", "+0.524944", "+0.536624", "+30.581575", "+30.581722", "+40.103312", "+40.111020"]

describe("ladderDelta", () => {
  it("reads two runs of one call as no delta: titles, times and CSeq bases fold", () => {
    const before = ladder("run1 c-case1", rows(603448825, REAL))
    const after = ladder("run2 c-case1", rows(1, FAKE))
    expect(ladderDelta(before, after)).toEqual({ firstDifference: null, differingLines: 0 })
  })

  it("states a missing ACK at the event it moves, as the two normalised lines, counted once", () => {
    const before = ladder("run1 c-case1", rows(603448825, REAL))
    const after = ladder("run2 c-case1", rows(1, FAKE, { ack: false }))
    expect(ladderDelta(before, after)).toEqual({
      firstDifference: {
        line: 5,
        before: "SUT -> uas1 | ACK | CSeq +0 | fromB1 · toB1 | s6",
        after: "SUT -> uac1 | OPTIONS | CSeq +0 | fromA1 · toA1 | A#1"
      },
      differingLines: 1
    })
  })

  it("states another status where it first shows, each changed line counted once", () => {
    const before = ladder("run1 c-case1", rows(603448825, REAL))
    const after = ladder("run2 c-case1", rows(1, FAKE, { final: "486 Busy Here" }))
    const delta = ladderDelta(before, after)
    expect(delta.firstDifference).toEqual({
      line: 2,
      before: "uas1 -> SUT | 200 OK | INVITE · CSeq +0 | fromB1 · toB1 | s3",
      after: "uas1 -> SUT | 486 Busy Here | INVITE · CSeq +0 | fromB1 · toB1 | s3"
    })
    expect(delta.differingLines).toBe(2)
  })

  it("states a ladder that runs out as a null line on its side", () => {
    const before = ladder("run1 c-case1", rows(1, FAKE))
    const after = ladder("run2 c-case1", rows(1, FAKE).slice(0, 8))
    expect(ladderDelta(before, after)).toEqual({
      firstDifference: { line: 8, before: "uac1 -> SUT | BYE | CSeq +1 | fromA1 · toA1 | s7", after: null },
      differingLines: 2
    })
  })
})

describe("normaliseLadder", () => {
  it("keeps the event lines alone, each without its index and time, CSeq relative to its space", () => {
    expect(normaliseLadder(ladder("run1 c-case1", rows(603448825, REAL)))).toEqual([
      "uac1 -> SUT | INVITE | CSeq +0 | fromA1 | s1",
      "SUT -> uas1 | INVITE | CSeq +0 | fromB1 | s2",
      "uas1 -> SUT | 200 OK | INVITE · CSeq +0 | fromB1 · toB1 | s3",
      "SUT -> uac1 | 200 OK | INVITE · CSeq +0 | fromA1 · toA1 | s4",
      "uac1 -> SUT | ACK | CSeq +0 | fromA1 · toA1 | s5",
      "SUT -> uas1 | ACK | CSeq +0 | fromB1 · toB1 | s6",
      "SUT -> uac1 | OPTIONS | CSeq +0 | fromA1 · toA1 | A#1",
      "uac1 -> SUT | 200 OK | OPTIONS · CSeq +0 | fromA1 · toA1 | A#2",
      "uac1 -> SUT | BYE | CSeq +1 | fromA1 · toA1 | s7",
      "SUT -> uas1 | BYE | CSeq +1 | fromB1 · toB1 | s8"
    ])
  })

  it("keeps an inject event and a missing time", () => {
    expect(normaliseLadder([
      "t",
      "",
      "ladder (2 events; m<idx> +t from -> to | message | detail | dialog | step):",
      "  m0 +-.------ inject hangup uac1",
      "  m1 +0.000000 uac1 -> SUT | BYE | CSeq 9 | fromA1 · toA1 | s1"
    ])).toEqual(["inject hangup uac1", "uac1 -> SUT | BYE | CSeq +0 | fromA1 · toA1 | s1"])
  })
})
