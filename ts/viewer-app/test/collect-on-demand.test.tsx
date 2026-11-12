/**
 * What a page asks of the server for an on-demand run and a campaign start:
 * the call details ask the lane's on-demand collectors and run one under the
 * lane id the page shows, as the lane run states it, never a form of it; a
 * start offers the collection levels the server states past `standard`, the
 * blank choice naming the server's default, and offers none where
 * `standard` is the only level.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { afterEach, describe, expect, it, vi } from "vitest"
import { api, type CampaignLane } from "../src/api.ts"
import { CampaignStart } from "../src/components/CampaignLaunch.tsx"
import { collectCellOf } from "../src/components/attachments/CollectOnDemand.tsx"

const LANE_RUN: CampaignLane = {
  lane: "rig-sut-a",
  runId: 41,
  session: "run41",
  status: "done",
  detail: null,
  skipped: 0,
  excluded: [],
  results: []
}

/** Each request the page made: its URL and its JSON body, where it sent one. */
const asked: Array<{ readonly url: string; readonly body: unknown }> = []

const answering = (body: unknown): void => {
  vi.stubGlobal("fetch", (url: string, init?: RequestInit) => {
    asked.push({ url, body: init?.body === undefined ? undefined : JSON.parse(String(init.body)) })
    return Promise.resolve(new Response(JSON.stringify(body), { status: 200 }))
  })
}

afterEach(() => {
  vi.unstubAllGlobals()
  asked.length = 0
})

describe("an on-demand run from the call details", () => {
  it("asks the offered collectors and runs one under the lane id the page shows", async () => {
    const cell = collectCellOf("capture-a", LANE_RUN, "cell-1", "case-1 [s]")
    answering(["log-again"])
    await api.collectOffered(cell.lane)
    answering({ verifications: [], attached: 1, leftChanged: [] })
    await api.collect({ ...cell, collector: "log-again" })
    expect(asked.map((one) => one.url)).toEqual(["/api/collect?lane=rig-sut-a", "/api/collect"])
    expect(asked[1]!.body).toEqual({
      capture: "capture-a",
      session: "run41",
      cell: "cell-1",
      runId: 41,
      caseId: "case-1 [s]",
      lane: "rig-sut-a",
      collector: "log-again"
    })
  })
})

describe("a campaign start's collection level", () => {
  const start = (collection: { readonly default: string; readonly levels: readonly string[] } | undefined) =>
    renderToStaticMarkup(
      <CampaignStart
        capture="campaign-a"
        lanes={[{ lane: "rig-sut-a", group: "group-1", refused: null }]}
        stated={null}
        tags={[]}
        cases={[]}
        collection={collection}
        open
      />
    )

  it("offers every level the server states, the blank choice naming its default", () => {
    const markup = start({ default: "lean", levels: ["standard", "lean", "full"] })
    expect(markup).toContain(`<option value="" selected="">lean (default)</option>`)
    expect(markup).toContain(`<option value="standard">standard</option>`)
    expect(markup).toContain(`<option value="lean">lean</option>`)
    expect(markup).toContain(`<option value="full">full</option>`)
  })

  it("offers no level where standard is the only one, or the server states none", () => {
    expect(start({ default: "standard", levels: ["standard"] })).not.toContain("<select")
    expect(start(undefined)).not.toContain("<select")
  })
})
