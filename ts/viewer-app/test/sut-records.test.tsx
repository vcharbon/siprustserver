/**
 * The SUT's records of one cell: its minted Call-IDs and window, each SUT log
 * collected over that window, a log no line falls in stated as such, and the
 * call state the SUT still held for the cell's calls.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { SutRecords } from "../src/components/SutRecords.tsx"

const JOIN = { callIds: ["A-1@pivot.invalid"], from: "2026-10-14T09:00:10.000Z", to: "2026-10-14T09:00:12.000Z" }

describe("SutRecords", () => {
  it("names the minted Call-IDs and the window, and draws each SUT log of the cell", () => {
    const sutLogs = [
      { title: "sut log", text: "09:00:11 INVITE A-1@pivot.invalid\n", bytes: 34 },
      { title: "quiet log", text: "", bytes: 0 }
    ]
    const html = renderToStaticMarkup(<SutRecords join={JOIN} sutLogs={sutLogs} held={[]} />)
    expect(html).toContain("A-1@pivot.invalid")
    expect(html).toContain("2026-10-14T09:00:10.000Z → 2026-10-14T09:00:12.000Z")
    expect(html).toContain("sut log · this cell")
    expect(html).toContain("09:00:11 INVITE")
    expect(html).toContain("no line in this window")
    expect(html).not.toContain("still held")
  })

  it("says what the SUT still held of the cell's calls, and that no log was collected", () => {
    const html = renderToStaticMarkup(<SutRecords join={JOIN} sutLogs={[]} held={[{ ref_id: "sha1" }]} />)
    expect(html).toContain("call state held after the batch")
    expect(html).toContain("sha1")
    expect(html).toContain("no SUT log collected")
  })
})
