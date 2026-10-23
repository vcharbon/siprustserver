/**
 * The preview pane: one line per lane, the captures each with its kind, by
 * name, the list folded past a handful.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { RequestLanePreview, RequestPreview } from "../src/api.ts"
import { RequestPreviewPane } from "../src/components/RequestPreview.tsx"

const A = "capture-a.pcap"
const B = "A103-19_145344_sip_vHKH.pcapng.gz"
const C = "A101-01_101010_sip_vHKH.pcapng.gz"

const preview: RequestPreview = {
  prepId: 2,
  lanes: [
    {
      lane: "fake",
      cells: 44,
      captures: [
        { capture: A, cells: 41, cut: 4862, kind: "cases" },
        { capture: B, cells: 1, cut: 1, kind: "full" },
        { capture: C, cells: 2, cut: 3, kind: "cases" }
      ],
      whole: [B],
      unkeyed: 0
    }
  ]
}

const render = (shown: RequestPreview): string => renderToStaticMarkup(<RequestPreviewPane preview={shown} />)

describe("the preview pane", () => {
  it("reads one line per lane and lists its captures by name, each with its kind", () => {
    const html = render(preview)
    expect(html).toContain("against prep 2")
    expect(html).toContain("<b>fake</b>: 44 cell(s) over 3 capture(s) — 1 full, 2 as cases")
    expect(html).toContain(`<li>${B} <span class="tag">full</span> 1 cell(s)</li>`)
    expect(html.indexOf(`${C} <span`)).toBeLessThan(html.indexOf(`${A} <span`))
    expect(html).not.toContain("<details")
  })

  it("folds a long capture list behind its count", () => {
    const many: RequestLanePreview = {
      lane: "lab",
      cells: 30,
      captures: Array.from({ length: 30 }, (_, i) => ({
        capture: `A1${String(i).padStart(2, "0")}_sip_vHKH.pcapng.gz`,
        cells: 1,
        cut: 1,
        kind: "full" as const
      })),
      whole: [],
      unkeyed: 0
    }
    const html = render({ prepId: 2, lanes: [many] })
    expect(html).toContain('<details class="request-fold"><summary>30 captures</summary>')
  })
})
