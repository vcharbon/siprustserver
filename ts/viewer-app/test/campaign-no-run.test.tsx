/**
 * The page of a campaign with no run yet: it says so, and offers the start
 * of its first run as a page with runs offers it, open: every lane the server
 * names, a refused one disabled with the server's reason, and the run
 * control. A page that reads another environment than the server's offers no
 * start, and its environment picker offers the server's own.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { Campaign } from "../src/api.ts"
import { CampaignEnvPick } from "../src/components/CampaignEnvPick.tsx"
import { CampaignNoRun } from "../src/components/CampaignNoRun.tsx"

const REFUSED = "lane sut-b runs only in the shared environment"

const view: Campaign = {
  capture: "e2e",
  env: null,
  envs: [],
  saidLabel: "service said",
  sides: [],
  lanes: [],
  runs: [],
  cases: [],
  own: "env-own",
  launch: {
    lanes: [
      { lane: "sut-a", group: "group-1", refused: null },
      { lane: "sut-b", group: "group-1", refused: REFUSED }
    ]
  }
}

describe("a campaign with no run", () => {
  it("says it has no run yet and offers its first run on every lane, open", () => {
    const markup = renderToStaticMarkup(<CampaignNoRun view={view} />)
    expect(markup).toContain("e2e")
    expect(markup).toContain("no run yet")
    expect(markup).toMatch(/<details[^>]* open/)
    expect(markup).toContain("sut-a")
    expect(markup).toContain("sut-b")
    expect(markup).toContain(REFUSED)
    expect(markup).toMatch(/<input[^>]*disabled[^>]*>sut-b/)
    expect(markup).toContain("run campaign")
  })

  it("offers no start where the server does not run the environment read", () => {
    const markup = renderToStaticMarkup(<CampaignNoRun view={{ ...view, env: "env-b", envs: [], launch: null, own: null }} />)
    expect(markup).toContain("no run yet")
    expect(markup).not.toContain("run campaign")
  })

  it("says the environment read has no run where another one ran it, and offers that one", () => {
    const markup = renderToStaticMarkup(<CampaignNoRun view={{ ...view, env: "env-b", envs: ["env-a"], launch: null, own: null }} />)
    expect(markup).toContain("no run in this environment")
    expect(markup).not.toContain("no run yet")
    expect(markup).toContain("<option value=\"env-a\">env-a</option>")
  })

  it("offers the environment the server starts runs in, though only another one wrote runs", () => {
    const read = { ...view, env: "env-a", envs: ["env-a"], launch: null }
    const markup = renderToStaticMarkup(<CampaignEnvPick view={read} />)
    expect(markup).toContain("<option value=\"env-own\">env-own</option>")
    expect(markup).toContain("<option value=\"env-a\" selected=\"\">env-a</option>")
    expect(renderToStaticMarkup(<CampaignEnvPick view={{ ...read, own: null }} />)).toBe("")
  })
})
