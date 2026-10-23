/**
 * The SUT's records of one cell: its minted Call-IDs and window, each hook
 * log cut to that window, a window no line falls in stated as such, and the
 * call state the SUT still held for the cell's calls.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { SutRecords } from "../src/components/SutRecords.tsx"

const JOIN = { callIds: ["A-1@pivot.invalid"], from: "2026-10-14T09:00:10.000Z", to: "2026-10-14T09:00:12.000Z" }

describe("SutRecords", () => {
  it("names the minted Call-IDs and the window, and draws each log cut to it", () => {
    const html = renderToStaticMarkup(
      <SutRecords join={JOIN} windows={[{ name: "sut-logs", text: "09:00:11 INVITE A-1@pivot.invalid\n" }, { name: "quiet", text: "" }]} held={[]} />
    )
    expect(html).toContain("A-1@pivot.invalid")
    expect(html).toContain("2026-10-14T09:00:10.000Z → 2026-10-14T09:00:12.000Z")
    expect(html).toContain("hook: sut-logs, this cell&#x27;s window")
    expect(html).toContain("09:00:11 INVITE")
    expect(html).toContain("(no line of the log falls inside this window)")
    expect(html).not.toContain("still held")
  })

  it("says what the SUT still held of the cell's calls, and that no log was kept", () => {
    const html = renderToStaticMarkup(<SutRecords join={JOIN} windows={[]} held={[{ ref_id: "sha1" }]} />)
    expect(html).toContain("the SUT still held call state of this cell after its batch")
    expect(html).toContain("sha1")
    expect(html).toContain("no hook log is kept with this run")
  })
})
