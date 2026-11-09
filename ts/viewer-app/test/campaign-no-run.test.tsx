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
  title: null,
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
    ],
    tags: [],
    defaultLanes: null
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

  it("offers a tag a lane it opens with excludes, unchecked, and no tag no such lane excludes", () => {
    const tag = (name: string, excludedOn: readonly string[]) => ({ tag: name, label: `Label ${name}`, warning: "Warned", excludedOn, allowed: false })
    const launch = { ...view.launch!, tags: [tag("left", ["sut-a"]), tag("free", ["sut-c"])] }
    const markup = renderToStaticMarkup(<CampaignNoRun view={{ ...view, launch }} />)
    expect(markup).toMatch(/<input type="checkbox"\/>Label left/)
    expect(markup).not.toContain("Label left (")
    expect(markup).not.toContain("Label free")
    expect(markup).not.toContain("Warned")
  })

  it("names itself by its title and opens on the lanes it states", () => {
    const lanes = [{ lane: "sut-a", group: "group-1", refused: null }, { lane: "sut-c", group: "group-2", refused: null }]
    const markup = renderToStaticMarkup(
      <CampaignNoRun view={{ ...view, title: "Second campaign", launch: { lanes, tags: [], defaultLanes: ["sut-c"] } }} />
    )
    expect(markup).toContain("<h1>Second campaign</h1>")
    expect(markup).toMatch(/<input type="checkbox"\/>sut-a/)
    expect(markup).toMatch(/<input type="checkbox" checked=""\/>sut-c/)
  })

  it("shows a tag the campaign opts into checked and fixed, with its warning", () => {
    const tags = [{ tag: "own", label: "Label own", warning: "Warned", excludedOn: ["sut-a"], allowed: true }]
    const markup = renderToStaticMarkup(<CampaignNoRun view={{ ...view, launch: { ...view.launch!, tags } }} />)
    expect(markup).toMatch(/<input type="checkbox" disabled="" checked=""\/>Label own/)
    expect(markup).toContain("Warned")
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
