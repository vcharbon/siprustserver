/**
 * Planning a replay off the cut store, against a real postgres.
 *
 * The claim under test is the one that makes a mid-sweep re-cut harmless: a
 * command resolves ONE prep and reads every capture it touches at that prep, so a
 * re-cut landing mid-sweep cannot hand half a sweep a different reading. The
 * rest is the vocabulary — a broken cut, a cut that refused every case and a
 * capture nobody cut are three answers, not one silence.
 *
 * The suite builds and drops its OWN database, so the box's own store is
 * never touched (`@sip/store/test-server`).
 */
import { Client, Cuts, Model, Refusals } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest"
import * as Content from "../../src/content/content.js"
import { CaseLayout } from "../../src/deployment/index.js"
import * as Plan from "../../src/replay/plan.js"
import type { Work } from "../../src/universe.js"

const SCRATCH = await scratch("corpus_plan_test")

/** The default side-row layout over the platform. */
const host = Layer.merge(CaseLayout.layer, NodeServices.layer)

const withCuts = <A, E>(
  effect: Effect.Effect<A, E, Cuts.Service | Refusals.Service | SqlClient | Layer.Success<typeof host>>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Cuts.layer, Refusals.layer, host]),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string, number: string): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@key`,
  document: { case: { id }, actors: [{ kind: "uas", endpoint: "ep2" }] },
  assets: [{ path: "resources/uac1_0_0.sdp", bytes: bytes(`v=0\r\no=${id}\r\n`) }],
  ledger: [{ number, document: { number } }],
  egressEndpoint: "ep2"
})

const CUT = "capture_cut.pcap.gz"
const RECUT = "capture_recut.pcap.gz"
const BROKE = "capture_broke.pcap.gz"
const REFUSED = "capture_refused.pcap.gz"
const NEVER = "capture_never.pcap.gz"

/** The two preps the suite reads at: an older one, and a re-cut of one capture. */
let older = 0
let newer = 0
let sessions: string

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
  await withCuts(
    Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      older = yield* cuts.open()
      yield* cuts.record({
        prepId: older,
        capture: CUT,
        flows: Model.wholeFlows(bytes(`{"capture":"${CUT}"}`)),
        cases: [caseOf(`${CUT}-auto`, "1001")],
        refusals: []
      })
      yield* cuts.record({
        prepId: older,
        capture: RECUT,
        flows: Model.wholeFlows(bytes(`{"capture":"${RECUT}"}`)),
        cases: [caseOf(`${RECUT}-auto`, "1002")],
        refusals: []
      })
      yield* cuts.record({
        prepId: older,
        capture: BROKE,
        flows: null,
        broke: "[prep] enrich failed",
        cases: [],
        refusals: []
      })
      yield* cuts.record({
        prepId: older,
        capture: REFUSED,
        flows: Model.wholeFlows(bytes(`{"capture":"${REFUSED}"}`)),
        cases: [],
        refusals: []
      })

      // A later cut of ONE capture: what a pinned command must not pick up.
      newer = yield* cuts.open()
      yield* cuts.record({
        prepId: newer,
        capture: RECUT,
        flows: Model.wholeFlows(bytes(`{"capture":"${RECUT}","again":true}`)),
        cases: [caseOf(`${RECUT}-auto`, "1002"), caseOf(`${RECUT}-b`, "1003")],
        refusals: []
      })
    })
  )
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

beforeEach(() => {
  sessions = fs.mkdtempSync(path.join(os.tmpdir(), "plan-test-"))
})
afterEach(() => {
  fs.rmSync(sessions, { recursive: true, force: true })
})

const whole = (capture: string): Work => ({ capture, set: { kind: "full" } })
const some = (capture: string, cases: ReadonlyArray<string>): Work => ({ capture, set: { kind: "cases", cases } })

const planned = (prepId: number, captures: ReadonlyArray<string | Work>) =>
  withCuts(
    Plan.planWork(
      captures.map((entry) => (typeof entry === "string" ? whole(entry) : entry)),
      prepId,
      2
    )
  )

/** The plans' cells materialised, as a batch does it. */
const materialised = (plans: ReadonlyArray<Plan.CapturePlan>) =>
  withCuts(
    Plan.materialiseBatch({
      cells: Plan.cellsOf(plans),
      lane: "paused",
      sessions,
      flowsHash: (capture) => {
        const plan = plans.find((entry) => entry.capture === capture)
        return plan?._tag === "cells" ? plan.flowsHash : null
      },
      concurrency: 2
    })
  )

/** The case hashes materialised under the sessions' cache so far. */
const onDisk = (): ReadonlyArray<string> => {
  const cache = path.join(sessions, Content.CONTENT)
  return fs.existsSync(cache)
    ? fs.readdirSync(cache).filter((name) => !name.startsWith("_") && !name.startsWith("."))
    : []
}

describe.skipIf(!served)("the replay plan", () => {
  it("pins one prep for every capture the command touches", async () => {
    // The standing prep re-cut ONE capture; a command pinned to the older one
    // must read every capture there, including the one a newer prep re-cut.
    expect(await withCuts(Plan.pin())).toEqual(newer)
    expect(await withCuts(Plan.pin(older))).toEqual(older)
    expect(await withCuts(Plan.pin(older + newer + 1))).toBeNull()

    const plans = await planned(older, [CUT, RECUT])
    expect(plans.map((entry) => entry._tag)).toEqual(["cells", "cells"])
    expect(Plan.cellsOf(plans).map((cell) => cell.caseId)).toEqual([
      `${CUT}-auto`,
      `${RECUT}-auto`
    ])

    // The same captures at the standing prep see the re-cut's second case.
    const standing = await planned(newer, [RECUT])
    expect(Plan.cellsOf(standing).map((cell) => cell.caseId)).toEqual([
      `${RECUT}-auto`,
      `${RECUT}-b`
    ])
  })

  it("reads a capture the pinned prep did not cut as it stood before it", async () => {
    // The pin is a CEILING: the newer prep re-cut ONE capture, and a command
    // pinned to it must still see the others as the older prep left them —
    // a targeted re-cut does not un-cut the corpus behind it.
    const plans = await planned(newer, [CUT, RECUT, REFUSED])
    expect(plans.map((entry) => entry._tag)).toEqual(["cells", "cells", "no-cases"])
    expect(Plan.cellsOf(plans).map((cell) => cell.caseId)).toEqual([
      `${CUT}-auto`,
      `${RECUT}-auto`,
      `${RECUT}-b`
    ])
  })

  it("names cells and materialises nothing until a batch asks", async () => {
    const plans = await planned(older, [CUT])
    expect(Plan.cellsOf(plans)).toEqual([
      {
        capture: CUT,
        callKey: `${CUT}-auto@key`,
        caseId: `${CUT}-auto`,
        caseHash: expect.any(String),
        egressEndpoint: "ep2"
      }
    ])
    expect(onDisk()).toEqual([])
  })

  it("cites a materialised case directory named by the case's own id", async () => {
    const plans = await planned(older, [CUT])
    const [only] = (await materialised(plans)).cells
    const cell = only!.cell
    expect(path.basename(cell.case)).toEqual(`${CUT}-auto`)
    expect(cell.case).toEqual(path.join(Content.caseDir(sessions, only!.caseHash), `${CUT}-auto`))
    expect(cell.lane).toEqual("paused")
    expect(cell.egressEndpoint).toEqual("ep2")
    // What `Case::load` opens, materialised from rows alone.
    expect(fs.readlinkSync(path.join(cell.case, "scenario.json"))).toEqual(`${CUT}-auto.v3.json`)
    expect(fs.existsSync(path.join(cell.case, "resources/uac1_0_0.sdp"))).toBe(true)

    // The flows document the post-run confrontation reads, byte for byte.
    expect(fs.readFileSync(cell.flows!, "utf8")).toEqual(`{"capture":"${CUT}"}`)
  })

  it("plans the ids a cases set names, never the capture's cut, and refuses one the cut holds nowhere", async () => {
    const plans = await planned(newer, [some(RECUT, [`${RECUT}-b`])])
    expect(plans.map((entry) => entry._tag)).toEqual(["cells"])
    expect(Plan.cellsOf(plans).map((cell) => cell.caseId)).toEqual([`${RECUT}-b`])
    expect(plans[0]).toMatchObject({ kind: "cases" })
    // One case on disk once the batch materialised it: the named one, not the other beside it.
    await materialised(plans)
    expect(onDisk()).toEqual([Plan.cellsOf(plans)[0]!.caseHash])

    const exit = await withCuts(Effect.exit(Plan.planWork([some(RECUT, [`${RECUT}-b`, `${RECUT}-nope`])], newer, 2)))
    expect(exit._tag).toBe("Failure")
    expect(exit._tag === "Failure" ? String(exit.cause) : "").toContain(`${RECUT}-nope`)
  })

  it("refuses each id the pinned cut holds no case of with one token and one reason", async () => {
    // The older prep never cut `-b`; the newer one did. Pinned to the older, `-b` is another cut's.
    const refused = await withCuts(
      Plan.planWork([some(RECUT, [`${RECUT}-b`, `${RECUT}-nope`])], older, 2).pipe(Effect.flip)
    )
    expect(refused._tag).toBe("ReplayDb.UnknownCases")
    const unknown = refused as Plan.UnknownCases
    expect(unknown.absences.map((absence) => [absence.caseId, absence.token])).toEqual([
      [`${RECUT}-b`, "not-in-this-cut"],
      [`${RECUT}-nope`, "unknown-id"]
    ])
    expect(unknown.message.split("\n").slice(1)).toEqual([
      `  ${RECUT}-b  not-in-this-cut: ${unknown.absences[0]!.reason}`,
      `  ${RECUT}-nope  unknown-id: ${unknown.absences[1]!.reason}`
    ])
  })

  it("plans a full set whole", async () => {
    const plans = await planned(newer, [RECUT])
    expect(plans[0]).toMatchObject({ kind: "full" })
    expect(Plan.cellsOf(plans)).toHaveLength(2)
    await materialised(plans)
    expect(onDisk()).toHaveLength(2)
  })

  it("keeps a broken cut, a refused one and an uncut capture apart", async () => {
    const plans = await planned(older, [BROKE, REFUSED, NEVER])
    expect(plans.map((entry) => entry._tag)).toEqual(["broke", "no-cases", "broke"])
    expect(plans[0]).toMatchObject({ broke: "[prep] enrich failed" })
    expect(plans[2]).toMatchObject({ broke: expect.stringContaining("no cut of this capture") })
  })

  it("makes a case that cannot be materialised that capture's failure alone", async () => {
    // The cache cannot be written, so every case fails to materialise — and the
    // capture beside it, whose cut refused everything, still answers `no-cases`.
    const cache = path.join(sessions, Content.CONTENT)
    fs.mkdirSync(cache, { recursive: true })
    fs.chmodSync(cache, 0o500)
    try {
      const plans = await planned(older, [CUT, REFUSED])
      expect(plans.map((entry) => entry._tag)).toEqual(["cells", "no-cases"])
      const batch = await materialised(plans)
      expect(batch.cells).toEqual([])
      expect(batch.broke.get(CUT)).toContain("materialising")
    } finally {
      fs.chmodSync(cache, 0o700)
    }
  })
})
