/**
 * A run's lanes under the pack's id prefix: a lane named by its id or its
 * short form, a list of them read or refused, the campaign it takes them from,
 * and the simulated lane's pacing and endpoints.
 */
import { LaneCatalog, LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import { campaignRunOf } from "../src/campaign-run.js"
import { catalogLane, endpointAddresses, laneNamed, lanesIn, lanesOf, pacingOf, refusalOf, resolve } from "../src/lanes.js"

const PREFIX = "pack-"
const SIM = LaneView.simulatedBuiltIn("pack-sim", {})
const OWN = LaneView.simulatedBuiltIn("lab", {})
const CATALOG = { lanes: [SIM, OWN] } as unknown as LaneCatalog.Catalog

describe("a run's lanes under the pack's prefix", () => {
  it("name a prefixed id by its short form and an id the catalog holds as itself", () => {
    expect(lanesOf(PREFIX, CATALOG)).toEqual(["sim", "lab"])
    expect([resolve(PREFIX, CATALOG, "sim"), resolve(PREFIX, CATALOG, "pack-sim"), resolve(PREFIX, CATALOG, "lab")])
      .toEqual(["pack-sim", "pack-sim", "lab"])
    expect(laneNamed(PREFIX, CATALOG, "pack-sim")).toBe("sim")
    expect(catalogLane(PREFIX, CATALOG, "sim")?.id).toBe("pack-sim")
  })

  it("read a list of lanes, refusing one the catalog does not hold with the lanes it does", () => {
    expect(lanesIn(PREFIX, "sim, lab", CATALOG)).toEqual(["sim", "lab"])
    expect(lanesIn(PREFIX, "sim,other", CATALOG)).toBe("unknown lane(s) other; lanes: sim, lab")
    expect(lanesIn(PREFIX, " , ", CATALOG)).toBe("no lane named")
  })

  it("refuse a campaign on a lane lacking a capability the pack's campaign needs", () => {
    expect(refusalOf(["no-such-capability"], SIM)).toContain("no-such-capability")
    expect(refusalOf([], SIM)).toBeUndefined()
  })

  it("run the simulated lane's cells four at once on the document's own endpoints", () => {
    expect(pacingOf(SIM)).toEqual({ concurrency: 4 })
    expect(endpointAddresses(SIM, ["alice"])).toEqual({})
  })
})

describe("a run's campaign", () => {
  const campaign = { name: "c", title: null, cases: ["**/*.ts"], lanes: ["sim"], allow: ["slow"], setup: [], coverage: false }

  it("takes its lanes under the pack's prefix, its tags beside --allow's, and its coverage", () => {
    expect(campaignRunOf(PREFIX, campaign, "slow,wip", CATALOG)).toEqual({
      name: "c",
      cases: ["**/*.ts"],
      lanes: ["sim"],
      allow: ["slow", "wip"],
      coverage: false,
      setup: []
    })
  })

  it("is refused for a lane it names that the catalog does not hold, or a campaign not chosen", () => {
    expect(campaignRunOf(PREFIX, { ...campaign, lanes: ["gone"] }, undefined, CATALOG))
      .toBe("campaign c: unknown lane(s) gone; lanes: sim, lab")
    expect(campaignRunOf(PREFIX, "no campaign x", undefined, CATALOG)).toBe("no campaign x")
  })
})
