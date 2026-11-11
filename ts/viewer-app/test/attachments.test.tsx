/**
 * Attachments drawn by kind: a holder's list ordered by kind, each folded
 * under its title with its file a link away; a diff's quiet rows left out
 * until asked, its classes counted; records side by side, a column per record;
 * a table as its rows; a log's lines with their count and the whole a link
 * away.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { AttachmentListed } from "../src/api.ts"
import { AttachmentList, ordered } from "../src/components/attachments/AttachmentList.tsx"
import { DiffAttachment } from "../src/components/attachments/DiffAttachment.tsx"
import { LogAttachment } from "../src/components/attachments/LogAttachment.tsx"
import { RecordsAttachment } from "../src/components/attachments/RecordsAttachment.tsx"
import { TableAttachment } from "../src/components/attachments/TableAttachment.tsx"

const one = (id: number, kind: AttachmentListed["kind"]): AttachmentListed => ({
  id,
  name: `n${id}`,
  kind,
  title: `title ${id}`,
  source: "s"
})

describe("a holder's attachments", () => {
  it("are listed by kind, the holder's order kept within one", () => {
    const listed = [one(1, "log"), one(2, "records"), one(3, "link"), one(4, "records"), one(5, "json")]
    expect(ordered(listed).map((a) => a.id)).toEqual([2, 4, 5, 1, 3])
  })

  it("are each folded under its title, its file linked, a link having none", () => {
    const html = renderToStaticMarkup(<AttachmentList listed={[one(7, "log"), one(8, "link")]} heading="Attachments" />)
    expect(html).toContain("title 7")
    expect(html).toContain("href=\"/api/attachments/7/file\"")
    expect(html).not.toContain("/api/attachments/8/file")
    expect(renderToStaticMarkup(<AttachmentList listed={[]} heading="Attachments" />)).toContain("none")
  })
})

describe("a diff", () => {
  const diff = {
    columns: ["left 1", "right 1"],
    rows: [
      { group: "g · 1", field: "ELAPSED", values: ["12", "0"], class: "timing" },
      { group: "g · 1", field: "KIND", values: ["1", null], class: "content" }
    ],
    same: 4,
    quiet: ["timing"]
  }

  it("leaves its quiet rows out until asked, and counts each class and the fields alike", () => {
    const html = renderToStaticMarkup(<DiffAttachment diff={diff} own={0} />)
    expect(html).toContain("KIND")
    expect(html).not.toContain("ELAPSED")
    expect(html).toContain("—")
    expect(html).toContain("1 timing · 1 content · 4 same")
    expect(html).toContain("cp-own")
  })

  it("with no class counts its rows", () => {
    const html = renderToStaticMarkup(<DiffAttachment diff={{ columns: ["a", "b"], rows: [{ field: "F", values: ["1", "2"] }] }} />)
    expect(html).toContain("1 differ")
  })
})

describe("records, a table and a log", () => {
  it("draw records side by side, a column per record", () => {
    const html = renderToStaticMarkup(<RecordsAttachment records={[{ ID: "a", N: "1" }, { ID: "b" }]} />)
    expect(html).toContain("#1")
    expect(html).toContain("#2")
    expect(html.indexOf(">ID<")).toBeLessThan(html.indexOf(">N<"))
  })

  it("draw a table's rows under its columns", () => {
    const html = renderToStaticMarkup(<TableAttachment columns={["call", "seen"]} rows={[["c1", "2"]]} />)
    expect(html).toContain("<th>call</th>")
    expect(html).toContain("c1")
  })

  it("colour a table's cells by their class, a cell with none plain", () => {
    const html = renderToStaticMarkup(
      <TableAttachment
        columns={["call", "expected", "seen"]}
        rows={[["c1", "2", "2"], ["c2", "1", "0"], ["c3", "1", "—"]]}
        classes={[[null, null, "ok"], [null, null, "nok"], [null, null, "muted"]]}
      />
    )
    expect(html).toContain(`<td class="mono cp-ok">2</td>`)
    expect(html).toContain(`<td class="mono cp-nok">0</td>`)
    expect(html).toContain(`<td class="mono muted">—</td>`)
    expect(html).toContain(`<td class="mono">c2</td>`)
  })

  it("draw a log's lines with their count, the whole file a link away", () => {
    const html = renderToStaticMarkup(<LogAttachment text={"one\ntwo\n"} bytes={8} href="/api/attachments/3/file" />)
    expect(html).toContain("2/2 lines")
    expect(html).toContain("one\ntwo")
    expect(html).toContain("href=\"/api/attachments/3/file\"")
  })
})
