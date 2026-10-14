/**
 * The campaign's plan as a dry run prints it: one count per phase in phase
 * order, the replay line naming the lane and how a span bound split the work
 * list, add-only where no lane was named, and the first stale captures and
 * cases sets listed beneath with why.
 */
import { describe, expect, it } from "vitest"
import { type Plan, planLines } from "../src/campaign.js"
import { optionsOf } from "./cut/host.js"

const plan: Plan = {
  sources: [
    { capture: "a.pcap.gz", files: ["/p/a.pcap.gz"] },
    { capture: "b.pcap.gz", files: ["/p/b.pcap.gz"] }
  ] as unknown as Plan["sources"],
  collisions: [],
  missing: [],
  needIngest: [{ capture: "a.pcap.gz", why: "never extracted" }],
  needCut: [{ capture: "b.pcap.gz", why: "the cut code moved" }],
  populationSize: 7,
  replay: [
    { capture: "a.pcap.gz", set: { kind: "full" } },
    { capture: "b.pcap.gz", set: { kind: "cases", cases: ["b-1", "b-2"] } }
  ]
}

describe("the campaign's plan lines", () => {
  it("count each phase, then list what is stale and what replays as a cases set", () => {
    const options = optionsOf("/w")
    const bounded = { ...options, selection: { ...options.selection, maxSpanSeconds: 120 } }
    expect(planLines(plan, bounded, "paused")).toEqual([
      "[campaign] pcaps found           2",
      "[campaign] already ingested      1",
      "[campaign] needs ingest          1",
      "[campaign] already cut           1",
      "[campaign] needs cut             1",
      "[campaign] population            7",
      "[campaign] replay                2  after filters, on paused — 1 full, 1 as a cases set (max-span 120 s)",
      "   ingest  a.pcap.gz  never extracted",
      "   cut     b.pcap.gz  the cut code moved",
      "   cases   b.pcap.gz  a 2-case set: its calls under the span bound"
    ])
  })

  it("say add-only where no lane was named", () => {
    expect(planLines({ ...plan, replay: [] }, optionsOf("/w"), undefined)[6]).toBe(
      "[campaign] replay                0  (add-only: no lane)"
    )
  })
})
