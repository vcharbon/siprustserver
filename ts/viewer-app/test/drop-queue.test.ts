/**
 * What a drop of captures queues: each capture at the anonymisation level
 * picked when it was dropped, and the picker back at the workspace's default
 * after every drop, so a capture dropped after a lab capture taken at `none`
 * goes in at the workspace's default unless `none` is picked for it again.
 */
import { describe, expect, it } from "vitest"
import { drop, type Picking } from "../src/components/drop-queue.ts"

const START: Picking = { level: "", next: 0 }

describe("a drop of captures", () => {
  it("queues a capture dropped after one at none at the workspace's default", () => {
    const lab = drop({ ...START, level: "none" }, ["lab.pcapng"], true)
    expect(lab.queued).toEqual([{ id: 0, files: ["lab.pcapng"], level: "none" }])
    const production = drop(lab.picking, ["prod.pcap"], true)
    expect(production.queued).toEqual([{ id: 1, files: ["prod.pcap"], level: "" }])
  })

  it("queues every capture of one drop at the level picked for it, one each or as one set", () => {
    const each = drop({ ...START, level: "keep-sut" }, ["a.pcap", "b.pcap"], false)
    expect(each.queued.map((one) => [one.id, one.files, one.level])).toEqual([
      [0, ["a.pcap"], "keep-sut"],
      [1, ["b.pcap"], "keep-sut"]
    ])
    const set = drop({ ...each.picking, level: "shareable" }, ["tap.pcap", "tap.pcap1"], true)
    expect(set.queued).toEqual([{ id: 2, files: ["tap.pcap", "tap.pcap1"], level: "shareable" }])
    expect(set.picking).toEqual({ level: "", next: 3 })
  })
})
