/**
 * The lane catalog over a pack's reading: the built-in simulated lane stands
 * first, read from its own file where one stands; each hosted lane's view is
 * what its file states, sorted by order then id; a key the simulated lane's
 * in-process SUT fixes, a second simulated lane, an unknown known bug and the
 * reading's own refusal each refuse the catalog; and the lanes of one job
 * group by the shared parts they name.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import { afterEach, describe, expect, it } from "vitest"
import * as LaneCatalog from "../src/lane-catalog.js"
import * as LaneView from "../src/lane-view.js"
import { type Entry, pack, run, tree } from "./tree.js"

const made: Array<string> = []

const root = (files: Readonly<Record<string, Entry>>): string => {
  const dir = tree(files)
  made.push(dir)
  return dir
}

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A records store, shared by the lanes of a job, readable once it names a reader. */
const RECORDS_STORE = JSON.stringify({
  "x-siprig": { shared: true, capabilities: { "records-readable": { reader: true } } },
  type: "object",
  additionalProperties: false,
  required: ["address"],
  properties: { address: { type: "string" }, reader: { type: "string" } }
})

const PACK = (): string => root({ "pack.toml": pack(), "parts/records-store.schema.json": RECORDS_STORE })

/** A hosted lane on backend `mode`, `top` before its tables and `tail` after them. */
const HOSTED = (mode: string, top = "", tail = ""): string =>
  `schema = "lane.v1"\nclock = "real"\n${top}\n[sip]\nlifecycle = "standing"\ningress = "192.0.2.10:5060"\nendpoint_host = "192.0.2.1"\n` +
  `[backend]\nmode = "${mode}"\n${mode === "none" ? "" : `listen = "192.0.2.1:8080"\n`}` +
  `[backend.http_budget]\nnew_call_ms = 200\nfollow_up_ms = 300\ntransit_ms = 1\n${tail}`

const SIMULATED = (tail = ""): string => `schema = "lane.v1"\nclock = "simulated"\n${tail}\n`

const STORES = { "targets/records-store/one.toml": `address = "192.0.2.20:6379"\n`, "targets/records-store/two.toml": `address = "192.0.2.21:6379"\n` }

/** What the test pack reads of a lane: the records store it names. */
interface Stored {
  readonly store: string | undefined
}

const storeOf = (lane: LaneView.View): Stored => ({ store: lane.loaded.parts.get("records-store")?.name })

/** The test pack's reading: the built-in lane `sim`, a hosted lane described `refused` refused by the pack. */
const reading = (packDir: string, reads: Array<string> = []): LaneCatalog.Reading<LaneView.Simulated & Stored, LaneView.Hosted & Stored, string> => ({
  pack: packDir,
  simulatedId: "sim",
  simulatedTimers: { keepalive_ms: 30_000 },
  simulated: (lane) => Effect.sync(() => (reads.push(lane.id), { ...lane, ...storeOf(lane) })),
  hosted: (lane) => (lane.sip.description === "refused" ? Effect.fail("refused by the pack") : Effect.succeed({ ...lane, ...storeOf(lane) }))
})

const loadOf = (files: Readonly<Record<string, Entry>>, reads?: Array<string>) =>
  run(LaneCatalog.load(reading(PACK(), reads), LaneCatalog.dirsUnder(root(files)), {}))

const catalogOf = async (files: Readonly<Record<string, Entry>>, reads?: Array<string>) => {
  const result = await loadOf(files, reads)
  if (Result.isFailure(result)) throw new Error(String(result.failure))
  return result.success
}

const reasonOf = async (files: Readonly<Record<string, Entry>>): Promise<string> => {
  const result = await loadOf(files)
  if (Result.isSuccess(result)) throw new Error("the catalog loaded")
  return typeof result.failure === "string" ? result.failure : result.failure.message
}

describe("the lane catalog", () => {
  it("holds the built-in simulated lane alone where no lane file stands, read by the pack once", async () => {
    const reads: Array<string> = []
    const catalog = await catalogOf({}, reads)
    expect(LaneCatalog.ids(catalog)).toEqual(["sim"])
    expect(reads).toEqual(["sim"])
    const sim = LaneCatalog.find(catalog, "sim")!
    expect(sim).toMatchObject({ kind: "fake", file: undefined, sutTimers: { keepalive_ms: 30_000 }, tags: LaneView.NO_TAG_LISTS, store: undefined })
    expect([...sim.capabilities].sort()).toEqual(["paused-clock", "routing-visible", "scripted-routing"])
    expect(LaneCatalog.real(catalog, "sim")).toBeUndefined()
  })

  it("states the simulated lane first, then the hosted ones by order then id, each as its file states it", async () => {
    const catalog = await catalogOf({
      "lanes/b.toml": HOSTED("served", "order = 1"),
      "lanes/a.toml": HOSTED("proxied", `description = "a lab system"\nknown_bugs = ["provisional-rewrite-not-applied"]`),
      "lanes/c.toml": HOSTED(
        "none",
        "",
        `[pacing]\nconcurrency = 4\n[pacing.ceilings]\ncells = 10\n[timers]\nkeepalive_ms = 300000\n[tags]\nrun = ["long"]\nexclude = ["short"]\n`
      )
    })
    expect(LaneCatalog.ids(catalog)).toEqual(["sim", "b", "a", "c"])
    const a = LaneCatalog.real(catalog, "a")!
    expect(a).toMatchObject({ kind: "real", backendMode: "proxied", backendListen: "192.0.2.1:8080", sutTimers: {}, tags: LaneView.NO_TAG_LISTS })
    expect(a.sip).toEqual({
      name: "a",
      file: a.file,
      description: "a lab system",
      lifecycle: "standing",
      ingress: "192.0.2.10:5060",
      endpoint_host: "192.0.2.1",
      known_bugs: ["provisional-rewrite-not-applied"],
      http_budget: { new_call_ms: 200, follow_up_ms: 300, transit_ms: 1 }
    })
    expect([...a.capabilities].sort()).toEqual(["real-clock", "routing-visible"])
    const c = LaneCatalog.real(catalog, "c")!
    expect(c).toMatchObject({ backendMode: "none", backendListen: undefined, sutTimers: { keepalive_ms: 300_000 }, tags: { run: ["long"], exclude: ["short"] } })
    expect(c.sip).toMatchObject({ description: "c", knobs: { concurrency: 4 }, ceilings: { cells: 10 } })
    expect(LaneCatalog.real(catalog, "b")!.sip.knobs).toBeUndefined()
  })

  it("reads the simulated lane's own file: its tags and the parts it names reach the pack's reading", async () => {
    const reads: Array<string> = []
    const catalog = await catalogOf({
      "lanes/sim.toml": SIMULATED(`[parts]\nrecords-store = "one"\n[tags]\nrun = ["long"]\n`),
      ...STORES
    }, reads)
    expect(reads).toEqual(["sim"])
    const sim = LaneCatalog.find(catalog, "sim")!
    expect(sim).toMatchObject({ kind: "fake", store: "one", tags: { run: ["long"], exclude: [] }, sutTimers: { keepalive_ms: 30_000 } })
    expect(sim.file).toMatch(/lanes\/sim\.toml$/)
    expect(sim.capabilities.has("records-readable")).toBe(false)
  })

  const refusals: ReadonlyArray<readonly [string, Readonly<Record<string, Entry>>, RegExp]> = [
    ["the built-in lane's id on a hosted lane", { "lanes/sim.toml": HOSTED("served") }, /sim\.toml: sim is the built-in lane: its clock is simulated$/],
    ["the built-in lane's file stating its SUT's timers", { "lanes/sim.toml": SIMULATED("[timers]\nkeepalive_ms = 1") }, /sim\.toml: timers: sim is the built-in lane, its SUT the in-process worker$/],
    ["the built-in lane's file stating a pacing", { "lanes/sim.toml": SIMULATED("[pacing]\nconcurrency = 1") }, /sim\.toml: pacing: sim is the built-in lane/],
    ["the built-in lane's file stating known bugs", { "lanes/sim.toml": SIMULATED(`known_bugs = ["provisional-rewrite-not-applied"]`) }, /sim\.toml: known_bugs: sim is the built-in lane/],
    ["the built-in lane's file stating an order", { "lanes/sim.toml": SIMULATED("order = 1") }, /sim\.toml: order: sim is the built-in lane/],
    ["a simulated lane other than the built-in one", { "lanes/x.toml": SIMULATED() }, /x\.toml: clock: the one simulated lane is the built-in sim$/],
    ["a known bug nobody defines", { "lanes/x.toml": HOSTED("served", `known_bugs = ["typo"]`) }, /x\.toml: known_bugs\..*Expected "provisional-rewrite-not-applied"$/],
    ["a lane file that is no TOML", { "lanes/x.toml": "sip = \n" }, /x\.toml: not TOML/],
    ["the pack's reading of a hosted lane", { "lanes/x.toml": HOSTED("served", `description = "refused"`) }, /^refused by the pack$/]
  ]
  for (const [what, files, reason] of refusals) {
    it(`is refused by ${what}`, async () => {
      expect(await reasonOf(files)).toMatch(reason)
    })
  }
})

describe("the lanes of one job", () => {
  const JOB = {
    "lanes/a.toml": HOSTED("served", "", `[parts]\nrecords-store = "one"\n`),
    "lanes/b.toml": HOSTED("served", "", `[parts]\nrecords-store = "two"\n`),
    "lanes/c.toml": HOSTED("served", "", `[parts]\nrecords-store = "one"\n`),
    ...STORES
  }

  it("group by the shared parts they name, each group and its lanes in the order first met", async () => {
    const catalog = await catalogOf(JOB)
    expect(LaneCatalog.groupOf(catalog, LaneCatalog.find(catalog, "b")!)).toBe("records-store two")
    expect(LaneCatalog.groupOf(catalog, LaneCatalog.find(catalog, "sim")!)).toBe("no records-store")
    const groups = LaneCatalog.groupsOf(catalog, catalog.lanes).map((group) => group.map((lane) => lane.id))
    expect(groups).toEqual([["sim"], ["a", "c"], ["b"]])
    expect(LaneCatalog.groupsLine(groups)).toBe("sim alone, or a + c, or b alone")
  })

  it("are one job's where they are of one group, else refused naming what each lane names", async () => {
    const catalog = await catalogOf(JOB)
    const lanes = (...ids: Array<string>) => ids.map((id) => LaneCatalog.find(catalog, id)!)
    const one = LaneCatalog.shared(catalog, lanes("a", "c"))
    expect(Result.isSuccess(one) && one.success.map((lane) => lane.store)).toEqual(["one", "one"])
    const mixed = LaneCatalog.shared(catalog, lanes("a", "b"))
    expect(Result.isFailure(mixed) && mixed.failure).toBe("the lanes of one job name one records-store part: a names one, b names two")
    const none = LaneCatalog.shared(catalog, lanes("sim", "a"))
    expect(Result.isFailure(none) && none.failure).toContain("sim names no records-store part")
  })
})
