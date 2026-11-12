/**
 * Whether a run fits a lane: it fits a lane holding what it needs, is refused
 * by one lacking any of it, runs forced and marked so; a check needing what
 * the lane lacks is not checkable there.
 */
import { describe, expect, it } from "vitest"
import * as Capability from "../src/capability.js"

describe("a run on a lane", () => {
  const bare = new Set(["real-clock", "routing-visible"])
  const readable = new Set([...bare, "records-readable"])
  const NEEDS = ["routing-visible", "records-readable"]

  it("fits a lane holding every capability it needs", () => {
    expect(Capability.fit("lab", readable, NEEDS, "a campaign", false)).toEqual({ _tag: "fits" })
  })

  it("is refused by a lane lacking one, naming what it lacks", () => {
    expect(Capability.fit("lab", bare, NEEDS, "a campaign", false)).toEqual({
      _tag: "refused",
      lacks: ["records-readable"],
      line: "a campaign is not runnable on this lane: lab lacks records-readable"
    })
    expect(Capability.fit("lab", readable, Capability.PCAP_REPLAY_NEEDS, "a capture replay", false)).toEqual({
      _tag: "refused",
      lacks: ["scripted-routing"],
      line: "a capture replay is not runnable on this lane: lab lacks scripted-routing"
    })
  })

  it("runs forced on a lane lacking one, its results marked forced", () => {
    expect(Capability.fit("lab", readable, Capability.PCAP_REPLAY_NEEDS, "a capture replay", true)).toEqual({
      _tag: "forced",
      lacks: ["scripted-routing"],
      line: "a capture replay is forced onto lab, which lacks scripted-routing: its results are marked forced"
    })
  })

  it("states a check needing a capability the lane lacks not checkable, and says nothing where it holds it", () => {
    expect(Capability.notCheckable("lab", bare, "records-readable", "records")).toBe("records: not checkable on lab, which lacks records-readable")
    expect(Capability.notCheckable("lab", readable, "records-readable", "records")).toBeUndefined()
  })
})
