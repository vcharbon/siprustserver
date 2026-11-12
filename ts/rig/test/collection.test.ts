/**
 * A job's wiring is checked against the rig path before it opens: a
 * collection level or a lane naming a collector the path does not define is
 * refused naming the file, and so is a level setting a `required` collector, a
 * lane excluding one, and a lane override of a key the collector does not
 * offer or of another type than its manifest's value. The appendix's own
 * wiring holds.
 */
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as Toml from "smol-toml"
import { describe, expect, it } from "vitest"
import * as Collection from "../src/collection.js"
import * as Collector from "../src/collector.js"
import type { Defined, RigPath } from "../src/rig-path.js"

const defined = (name: string, extra: Partial<Collector.Collector> = {}): Defined<Collector.Collector> => ({
  name,
  file: `pack/collectors/${name}/collector.toml`,
  dir: `pack/collectors/${name}`,
  level: "pack",
  manifest: { schema: Collector.SCHEMA, name, scope: "batch", run: { collect: "./collect" }, ...extra }
})

const RIG: RigPath = {
  pack: undefined,
  collectors: new Map([
    ["capture", defined("capture", { scope: "job" })],
    ["sut-log", defined("sut-log")],
    ["call-records", defined("call-records", { params: { poll_s: 1 } })],
    ["calls-ended", defined("calls-ended", { scope: "cell", delay_s: 10 })],
    ["records-chain", defined("records-chain", { required: true, params: { poll_s: 1 } })]
  ]),
  judges: new Map(),
  operations: new Map(),
  parts: new Map()
}

const collectionOf = (text: string) => Schema.decodeUnknownSync(Collection.Collection)(Toml.parse(text)["collection"])
const lanesOf = (text: string) => Schema.decodeUnknownSync(Collection.LaneCollectors)(Toml.parse(text)["collectors"])

const checked = (wiring: Collection.Wiring): string => {
  const result = Collection.check(RIG, wiring)
  return Result.isFailure(result) ? result.failure.message : "holds"
}

const workspace = (text: string) => ({ file: "rig.toml", collection: collectionOf(text) })
const lane = (text: string) => ({ file: "lanes/lab-a.toml", collectors: lanesOf(text) })

describe("the wiring of a job", () => {
  it("holds for the appendix's levels and lane", () => {
    const levels = workspace(`[collection]\ndefault = "standard"\n[collection.quick]\nnever = ["capture"]\n[collection.paranoid]\nalways = ["capture", "sut-log"]\n`)
    expect(checked({
      workspace: levels,
      level: "quick",
      lanes: [lane(`[collectors]\nexclude = ["capture"]\n[collectors.call-records]\npoll_s = 2\n[collectors.calls-ended]\ndelay_s = 40\n`)]
    })).toBe("holds")
    expect(Result.isSuccess(Collection.levelsOf(levels.collection)) && Collection.levelsOf(levels.collection)).toMatchObject({
      success: { default: "standard" }
    })
  })

  it("refuses a level naming a collector the path does not define", () => {
    expect(checked({ workspace: workspace(`[collection.quick]\nnever = ["pcap"]\n`), lanes: [] }))
      .toBe("rig.toml: collection.quick.never: names pcap, which no collector of the path defines")
  })

  it("refuses a level setting a required collector, naming its manifest too", () => {
    expect(checked({ workspace: workspace(`[collection.quick]\nnever = ["records-chain"]\n`), lanes: [] }))
      .toBe("rig.toml and pack/collectors/records-chain/collector.toml: collection.quick.never: names records-chain, a required collector no level sets")
  })

  it("refuses a level setting one collector twice", () => {
    expect(checked({ workspace: workspace(`[collection.quick]\nalways = ["capture"]\nnever = ["capture"]\n`), lanes: [] }))
      .toBe("rig.toml: collection.quick.never: names capture, which collection.quick.always names too")
  })

  it("refuses a default, or a job, naming a level the file does not define", () => {
    expect(checked({ workspace: workspace(`[collection]\ndefault = "quick"\n`), lanes: [] }))
      .toBe("rig.toml: collection.default: names quick, which is neither standard nor a level of the file")
    expect(checked({ workspace: workspace(`[collection.quick]\nnever = ["capture"]\n`), level: "deep", lanes: [] }))
      .toBe("rig.toml: the job asks for the collection level deep, which is neither standard nor a level of the file")
  })

  it("refuses a level that is no table", () => {
    expect(checked({ workspace: workspace(`[collection]\nquick = "never"\n`), lanes: [] })).toBe("rig.toml: collection.quick: a level is a table")
  })

  it("refuses a level named standard, each collector's own default", () => {
    expect(checked({ workspace: workspace(`[collection.standard]\nnever = ["capture"]\n`), lanes: [] }))
      .toBe("rig.toml: collection.standard: standard is each collector's own default, which no file defines")
  })

  it("refuses a lane excluding a collector the path does not define", () => {
    expect(checked({ lanes: [lane(`[collectors]\nexclude = ["pcap"]\n`)] }))
      .toBe("lanes/lab-a.toml: collectors.exclude: names pcap, which no collector of the path defines")
  })

  it("refuses a lane overriding a collector the path does not define", () => {
    expect(checked({ lanes: [lane(`[collectors.pcap]\npoll_s = 2\n`)] }))
      .toBe("lanes/lab-a.toml: collectors.pcap: names a collector the path does not define")
  })

  it("refuses a lane excluding a required collector, and lets it tune one", () => {
    expect(checked({ lanes: [lane(`[collectors]\nexclude = ["records-chain"]\n`)] }))
      .toBe("lanes/lab-a.toml and pack/collectors/records-chain/collector.toml: collectors.exclude: names records-chain, a required collector no lane excludes")
    expect(checked({ lanes: [lane(`[collectors.records-chain]\npoll_s = 5\n`)] })).toBe("holds")
  })

  it("refuses a lane override of a key the collector does not offer", () => {
    expect(checked({ lanes: [lane(`[collectors.call-records]\ndelay_s = 40\n`)] }))
      .toBe("lanes/lab-a.toml and pack/collectors/call-records/collector.toml: collectors.call-records.delay_s: not a parameter call-records offers")
  })

  it("refuses a lane override of another type than the manifest's value, naming both files", () => {
    expect(checked({ lanes: [lane(`[collectors.call-records]\npoll_s = "two"\n`)] }))
      .toBe("lanes/lab-a.toml and pack/collectors/call-records/collector.toml: collectors.call-records.poll_s: of type string, where call-records states one of type number")
    expect(checked({ lanes: [lane(`[collectors.calls-ended]\ndelay_s = [40]\n`)] }))
      .toBe("lanes/lab-a.toml and pack/collectors/calls-ended/collector.toml: collectors.calls-ended.delay_s: of type array, where calls-ended states one of type number")
  })

  it("reads a host file's tables, naming the key under the table at fault", () => {
    const refusal = <A>(result: Result.Result<A, string>) => (Result.isFailure(result) ? result.failure : "read")
    expect(refusal(Collection.collectionOf({ default: "quick", quick: { never: ["capture"] } }))).toBe("read")
    expect(refusal(Collection.collectionOf({ quick: { sometimes: ["capture"] } }))).toMatch(/^quick/)
    expect(refusal(Collection.laneCollectorsOf({ exclude: ["capture"], "call-records": { poll_s: 2 } }))).toBe("read")
    expect(refusal(Collection.laneCollectorsOf({ exclude: "capture" }))).toMatch(/^exclude/)
  })

  it("checks every lane of the job, naming the one at fault", () => {
    const good = lane(`[collectors]\nexclude = ["capture"]\n`)
    const bad = { file: "lanes/lab-b.toml", collectors: lanesOf(`[collectors]\nexclude = ["pcap"]\n`) }
    expect(checked({ lanes: [good, bad] })).toMatch(/^lanes\/lab-b\.toml: /)
  })
})
