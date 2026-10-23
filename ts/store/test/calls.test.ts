/**
 * The per-call queue, against a real postgres.
 *
 * What only the database can say: that the queue is the STANDING run's cases
 * and the STANDING cut's refusals, that an older run or cut of the same capture
 * is not there beside them, that the main error a writer stored comes back as
 * it went in, that the scope, the capture substring and every chip narrow the
 * page and the count in the statement itself, that the groups the chips are
 * counted from are the pool's, that a served row hydrates from the document
 * the run ran, and that a page is keyed through the cut's index once per
 * served row. The suite builds and drops its OWN database on the test server
 * (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Calls from "../src/calls.js"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Facets from "../src/facets.js"
import type { CaseResult, CutCaseRequest, MainError } from "../src/model.js"
import * as Population from "../src/population.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"
import { groupsOf } from "../src/memory-queue.js"

const SCRATCH = await Server.scratch("store_calls_test")
const LANE = "fake"

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Calls.Service | Cuts.Service | Runs.Service | Population.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Calls.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Population.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const DIED: MainError = {
  kind: "died",
  rule: null,
  rfc: null,
  step: "s14",
  expected: "200 to INVITE",
  detail: "gated on early dialog \"f5\""
}

const CHARGED: MainError = {
  kind: "rfc",
  rule: "rport-echo",
  rfc: "RFC 3581 §4",
  step: null,
  expected: null,
  detail: "response to a Via with rport echoes none — RFC 3581 §4"
}

const documentOf = (caseId: string, dialled: string): unknown => ({
  pivot_version: 3,
  case: { id: caseId, family: "transparent" },
  identities: [{ name: "callee", kind: "site", observed: dialled }],
  calls: [{
    id: "c1",
    caller_leg: "A",
    attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "callee" }, final: { status: 200, at_ms: 5 } }]
  }],
  legs: [{ id: "A" }, { id: "B" }]
})

const caseOf = (caseId: string, rest: Partial<CaseResult>): CaseResult => ({
  caseId,
  passed: false,
  verdict: "failed",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: documentOf(caseId, "+15550100123"),
  ...rest
})

const run = (capture: string, cases: ReadonlyArray<CaseResult>, setKind: "full" | "cases" = "full") =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({
      capture,
      lane: LANE,
      mode: "record",
      sessionDir: `/w/${capture}`,
      setKind,
      identity: CURRENT
    })
    yield* runs.finish({ id: opened.id, status: "done", cases })
    return opened
  })

const cut = (
  capture: string,
  refusals: ReadonlyArray<{ caseId: string; reason: string }>,
  cases: ReadonlyArray<CutCaseRequest> = []
) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const prep = yield* cuts.open()
    yield* cuts.record({
      prepId: prep,
      capture,
      flows: null,
      cases,
      refusals: refusals.map((one) => ({
        caseId: one.caseId,
        reasons: [one.reason],
        callIds: ["call-id-1"],
        detail: `${one.reason} on leg 1`
      }))
    })
    return prep
  })

const cutCase = (caseId: string): CutCaseRequest => ({
  caseId,
  callKey: `${caseId}@key`,
  document: documentOf(caseId, "+15550100123"),
  assets: [],
  ledger: [],
  egressEndpoint: null
})

beforeAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* Server.dropDatabase(SCRATCH)
      yield* sql.unsafe(`CREATE DATABASE ${SCRATCH}`)
    })
  )
  await on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      yield* Server.dropDatabase(SCRATCH)
    })
  )
}, 60_000)

describe.skipIf(!served)("Calls", () => {
  it("serves the source fault a writer stored: beside a failing call's main error, and on a passing call alone", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_source.pcap.gz"
      const sourceFault = { rule: "prack-2xx-or-481", rfc: "RFC 3262 §3", detail: "uas1: answered 481 — RFC 3262 §3" }
      const beside: MainError = { ...DIED, source: sourceFault }
      yield* run(capture, [
        caseOf(`${capture}-case1`, { failedStep: "s14", mainError: beside, sourceFault }),
        caseOf(`${capture}-case2`, { passed: true, verdict: "ok", rfcGating: 0, sourceFault }),
        caseOf(`${capture}-case3`, { failedStep: "s14", mainError: DIED })
      ])
      const queue = yield* calls.queue(LANE, { scope: "all", capture }, 0)
      expect(queue.rows.map((row) => [row.passed, row.mainError, row.sourceFault])).toEqual([
        [false, beside, sourceFault],
        [true, null, sourceFault],
        [false, DIED, undefined]
      ])
      // The source fault is a chip of its own, information beside the outcome.
      expect(queue.groups.filter((group) => group.source === "prack-2xx-or-481").map((group) => group.outcome).sort())
        .toEqual(["failed", "passed"])
      const narrowed = yield* calls.queue(LANE, { scope: "all", capture, source: "prack-2xx-or-481" }, 0)
      expect(narrowed.rows.map((row) => row.caseId)).toEqual([`${capture}-case1`, `${capture}-case2`])
      const hidden = yield* calls.queue(LANE, { scope: "all", capture, exclude: { source: ["prack-2xx-or-481"] } }, 0)
      expect(hidden.rows.map((row) => row.caseId)).toEqual([`${capture}-case3`])
    })))

  it("queues the standing run's cases and the standing cut's refusals, one row per call", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_three.pcap.gz"
      const opened = yield* run(capture, [
        caseOf(`${capture}-case1`, { failedStep: "s14", mainError: DIED }),
        caseOf(`${capture}-case2`, { verdict: "ok", rfcGating: 1, mainError: CHARGED })
      ])
      const prep = yield* cut(capture, [{ caseId: `${capture}-cut3`, reason: "egress-answer-not-captured" }])

      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows.map((row) => [row.caseId, row.kind, row.mainError?.kind])).toEqual([
        [`${capture}-case1`, "case", "died"],
        [`${capture}-case2`, "case", "rfc"],
        [`${capture}-cut3`, "refused", "refused"]
      ])

      const died = rows[0]!
      expect(died.runId).toEqual(opened.id)
      expect(died.bundlePath).toEqual(`/w/${capture}/run${opened.id}`)
      expect(died.passed).toBe(false)
      expect(died.failedStep).toEqual("s14")
      expect(died.mainError).toEqual(DIED)
      expect(died.finishedAt).not.toBeNull()

      const charged = rows[1]!
      expect(charged.rfcGating).toEqual(1)
      expect(charged.mainError).toEqual(CHARGED)

      const refused = rows[2]!
      expect(refused.runId).toBeNull()
      expect(refused.prepId).toEqual(prep)
      expect(refused.bundlePath).toBeNull()
      expect(refused.documentHash).toBeNull()
      expect(refused.reasons).toEqual(["egress-answer-not-captured"])
      expect(refused.mainError).toEqual({
        kind: "refused",
        rule: "egress-answer-not-captured",
        rfc: null,
        step: null,
        expected: null,
        detail: "egress-answer-not-captured on leg 1"
      })
      expect(died.reasons).toEqual([])
    })))

  it("leads a call refused under several reasons with the first, and carries them all", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const cuts = yield* Cuts.Service
      const population = yield* Population.Service
      const capture = "capture_two_reasons.pcap.gz"
      yield* population.add([{ capture, sourcePath: null }])
      const prep = yield* cuts.open()
      yield* cuts.record({
        prepId: prep,
        capture,
        flows: null,
        cases: [],
        refusals: [
          { caseId: `${capture}-auto`, reasons: ["egress-answer-not-captured"], callIds: ["a@x"], detail: "first" },
          { caseId: `${capture}-auto`, reasons: ["source-call-incomplete"], callIds: ["a@x"], detail: "second" }
        ]
      })
      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows).toHaveLength(1)
      expect(rows[0]!.reasons).toEqual(["egress-answer-not-captured", "source-call-incomplete"])
      expect(rows[0]!.mainError?.rule).toEqual("egress-answer-not-captured")
    })))

  it("serves a call the standing cut refused once, even where an older cut ran it as a case", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_recut_refused.pcap.gz"
      const caseId = `${capture}-auto`
      // Prep 1 cuts the call as a case and a full run fails it; prep 2 re-cuts
      // and refuses the same call; nothing re-runs. The standing cut says what
      // stands cut: the refusal, and the case row of the stale reading goes.
      yield* cut(capture, [], [cutCase(caseId)])
      const full = yield* run(capture, [caseOf(caseId, { mainError: DIED, failedStep: "s14" })])
      const recut = yield* cut(capture, [{ caseId, reason: "egress-answer-not-captured" }])

      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows.map((row) => [row.caseId, row.kind, row.runId, row.prepId])).toEqual([
        [caseId, "refused", null, recut]
      ])
      expect(full.id).toBeGreaterThan(0)
    })))

  it("names each row's call key: a case's from the cut, a refused call's the one its cut recorded, else the root of the Call-IDs it names", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const cuts = yield* Cuts.Service
      const capture = "capture_keyed.pcap.gz"
      const caseId = `${capture}-case1`
      const prep = yield* cuts.open()
      yield* cuts.record({
        prepId: prep,
        capture,
        flows: null,
        cases: [cutCase(caseId)],
        refusals: [{
          caseId: `${capture}-cut2`,
          reasons: ["source-call-incomplete"],
          callIds: ["1-SDroot@x", "SDroot@x"],
          detail: ""
        }, {
          // The key the cut recorded names the call, whatever the Call-IDs' order says.
          caseId: `${capture}-cut3`,
          reasons: ["source-call-incomplete"],
          callIds: ["aa-other@x", "zz-first@x"],
          detail: "",
          callKey: "zz-first@x"
        }]
      })
      yield* run(capture, [caseOf(caseId, { mainError: DIED, failedStep: "s14" }), caseOf(`${capture}-ghost`, {})])
      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows.map((row) => [row.caseId, row.callKey])).toEqual([
        [caseId, `${caseId}@key`],
        [`${capture}-ghost`, null],
        [`${capture}-cut2`, "SDroot@x"],
        [`${capture}-cut3`, "zz-first@x"]
      ])
    })))

  it("lists no refused call once a re-cut refused nothing", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const population = yield* Population.Service
      const capture = "capture_lifted.pcap.gz"
      yield* population.add([{ capture, sourcePath: null }])
      yield* cut(capture, [{ caseId: `${capture}-cut1`, reason: "source-call-incomplete" }])
      expect((yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows).toHaveLength(1)
      yield* cut(capture, [])
      expect((yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows).toHaveLength(0)
    })))

  it("orders cases before refused calls, then by capture and case id", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const population = yield* Population.Service
      const a = "capture_order_a.pcap.gz"
      const b = "capture_order_b.pcap.gz"
      yield* population.add([{ capture: a, sourcePath: null }, { capture: b, sourcePath: null }])
      yield* cut(a, [{ caseId: `${a}-cut1`, reason: "source-call-incomplete" }])
      yield* run(b, [caseOf(`${b}-case2`, { mainError: DIED }), caseOf(`${b}-case1`, { mainError: DIED })])
      yield* cut(b, [{ caseId: `${b}-cut3`, reason: "source-call-incomplete" }])
      const rows = (yield* calls.queue(LANE, { scope: "all", capture: "capture_order_" }, 0)).rows
      expect(rows.map((row) => row.caseId)).toEqual([`${b}-case1`, `${b}-case2`, `${a}-cut1`, `${b}-cut3`])
    })))

  it("lists the newest run and the newest cut of a capture, never an older one beside them", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_superseded.pcap.gz"
      yield* run(capture, [caseOf(`${capture}-case1`, { mainError: DIED }), caseOf(`${capture}-case2`, { mainError: DIED })])
      yield* cut(capture, [{ caseId: `${capture}-cut9`, reason: "source-truncated-at-edge" }])
      const newer = yield* run(capture, [caseOf(`${capture}-case1`, { mainError: CHARGED })])
      const newerCut = yield* cut(capture, [{ caseId: `${capture}-cut8`, reason: "egress-answer-not-captured" }])

      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows.map((row) => [row.caseId, row.runId, row.prepId])).toEqual([
        [`${capture}-case1`, newer.id, null],
        [`${capture}-cut8`, null, newerCut]
      ])
    })))

  it("shows a case at its current run, a cases run as much as a full one", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_rerun.pcap.gz"
      // A cases run alone is a run of the case it holds.
      const alone = yield* run(capture, [caseOf(`${capture}-case1`, { mainError: DIED })], "cases")
      expect((yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows.map((row) => row.runId)).toEqual([
        alone.id
      ])

      const full = yield* run(capture, [
        caseOf(`${capture}-case1`, { mainError: DIED, failedStep: "s14" }),
        caseOf(`${capture}-case2`, { mainError: CHARGED })
      ])
      const rerun = yield* run(capture, [caseOf(`${capture}-case1`, { passed: true, verdict: "ok" })], "cases")

      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(rows.map((row) => [row.caseId, row.runId, row.setKind, row.passed, row.mainError?.kind])).toEqual([
        [`${capture}-case1`, rerun.id, "cases", true, undefined],
        [`${capture}-case2`, full.id, "full", false, "rfc"]
      ])
      expect(rows[0]!.bundlePath).toEqual(`/w/${capture}/run${rerun.id}`)
    })))

  it("lists a refusal of a capture the population holds, and none of one nothing holds", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const population = yield* Population.Service
      yield* population.add([{ capture: "capture_member.pcap.gz", sourcePath: null }])
      yield* cut("capture_member.pcap.gz", [{ caseId: "capture_member.pcap.gz-cut1", reason: "source-call-incomplete" }])
      yield* cut("capture_stranger.pcap.gz", [{ caseId: "capture_stranger.pcap.gz-cut1", reason: "source-call-incomplete" }])

      const rows = (yield* calls.queue(LANE, { scope: "all" }, 0)).rows
      expect(rows.some((row) => row.caseId === "capture_member.pcap.gz-cut1")).toBe(true)
      expect(rows.some((row) => row.capture === "capture_stranger.pcap.gz")).toBe(false)
    })))

  it("keeps a run on another lane off this lane's listing", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const runs = yield* Runs.Service
      const capture = "capture_other_lane.pcap.gz"
      const opened = yield* runs.open({
        capture,
        lane: "ref",
        mode: "record",
        sessionDir: `/w/${capture}`,
        identity: CURRENT
      })
      yield* runs.finish({ id: opened.id, status: "done", cases: [caseOf(`${capture}-case1`, { mainError: DIED })] })
      expect((yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows).toHaveLength(0)
      expect((yield* calls.queue("ref", { scope: "all", capture }, 0)).rows).toHaveLength(1)
    })))

  it("reads a case's features off its document, whatever its outcome, and narrows and hides by them", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_featured.pcap.gz"
      const featured = (caseId: string, flags: ReadonlyArray<string>, flow: ReadonlyArray<unknown> = []) => ({
        ...(documentOf(caseId, "+15550100123") as Record<string, unknown>),
        case: { id: caseId, family: "reroute", annotations: { flags: flags.map((kind) => ({ kind, detail: "" })) } },
        flow
      })
      yield* run(capture, [
        caseOf(`${capture}-case1`, {
          passed: true,
          verdict: "ok",
          document: featured(`${capture}-case1`, ["detected:reroute", "detected-none:prack"])
        }),
        caseOf(`${capture}-case2`, {
          document: featured(`${capture}-case2`, ["detected:refer", "detected:fork", "detection-unavailable:mrf"])
        })
      ])

      // The baseline reading: every `detected:` annotation, by its name; no other outcome counts.
      const all = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      expect(all.map((row) => row.features)).toEqual([["reroute"], ["fork", "refer"]])
      const narrowed = yield* calls.queue(LANE, { scope: "all", capture, feature: "reroute" }, 0)
      expect(narrowed.rows.map((row) => row.caseId)).toEqual([`${capture}-case1`])
      const hidden = yield* calls.queue(LANE, { scope: "all", capture, exclude: { feature: ["fork"] } }, 0)
      expect(hidden.rows.map((row) => row.caseId)).toEqual([`${capture}-case1`])
      expect(narrowed.groups.flatMap((group) => group.features)).toContain("refer")
    })))

  it("hydrates a served row from the document the run ran, and a refused row to nulls", () =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      const capture = "capture_hydrated.pcap.gz"
      yield* run(capture, [caseOf(`${capture}-case1`, { passed: true, verdict: "ok" })])
      yield* cut(capture, [{ caseId: `${capture}-cut2`, reason: "egress-answer-not-captured" }])

      const rows = (yield* calls.queue(LANE, { scope: "all", capture }, 0)).rows
      const detail = yield* calls.hydrate(rows)
      expect(detail.map((row) => row.caseId)).toEqual(rows.map((row) => row.caseId))
      expect(detail[0]).toMatchObject({
        caseId: `${capture}-case1`,
        family: "transparent",
        dialled: "+15550100123",
        outcome: "200",
        legs: 2,
        attempts: 1
      })
      expect(detail[1]).toMatchObject({
        caseId: `${capture}-cut2`,
        family: null,
        dialled: null,
        outcome: null,
        legs: null,
        attempts: null
      })
      expect(yield* calls.hydrate([])).toEqual([])
    })))
})

const rfcOf = (rule: string, cite: string | null): MainError => ({
  kind: "rfc",
  rule,
  rfc: cite,
  step: null,
  expected: null,
  detail: `${rule} — ${cite ?? "uncited"}`
})

const diedOf = (step: string, expected: string): MainError => ({
  kind: "died",
  rule: null,
  rfc: null,
  step,
  expected,
  detail: `no ${expected}`
})

const failureOf = (tag: string): MainError => ({
  kind: "failure",
  rule: tag,
  rfc: null,
  step: null,
  expected: null,
  detail: tag
})

/** The interpreter's own refusal: a CASE row whose main error is of kind `refused`. */
const interpreterRefused = (reason: string): MainError => ({
  kind: "refused",
  rule: reason,
  rfc: null,
  step: null,
  expected: null,
  detail: reason
})

/**
 * The filter family: four captures under the substring `filt`, twelve calls,
 * every outcome, an RFC rule cited and uncited, two died calls waiting on the
 * same message, a failure tag, the interpreter's refusal, a call the cut
 * refused under one reason and one under two. The page orders by code unit
 * (`COLLATE "C"`), whatever the cluster's collation: the upper-case one first.
 */
const FA = "filt_a.pcap.gz"
const FB = "filt_b.pcap.gz"
const FV = "Filt231e.01_222706_sip_vHKH"
const FC = "filt_c.pcap.gz"
const Q = "filt"

const seedFilters = Effect.gen(function*() {
  const cuts = yield* Cuts.Service
  yield* run(FA, [
    caseOf(`${FA}-case1`, { mainError: rfcOf("rport-echo", "RFC 3581 §4") }),
    caseOf(`${FA}-case2`, { mainError: diedOf("s14", "200 to INVITE"), failedStep: "s14" }),
    caseOf(`${FA}-case3`, { passed: true, verdict: "ok" })
  ])
  yield* run(FB, [
    caseOf(`${FB}-case1`, { mainError: rfcOf("rport-echo", null) }),
    caseOf(`${FB}-case2`, { mainError: diedOf("s3", "BYE"), failedStep: "s3" }),
    caseOf(`${FB}-case3`, { verdict: "skipped", skipped: "blocked:no-mrf" })
  ])
  yield* cut(FB, [{ caseId: `${FB}-cut4`, reason: "egress-answer-not-captured" }])
  yield* run(FV, [
    caseOf(`${FV}-case1`, { mainError: diedOf("s14", "200 to INVITE"), failedStep: "s14" }),
    caseOf(`${FV}-case2`, { mainError: rfcOf("no-200-after-cancel", "RFC 3261 §9.1") })
  ])
  yield* run(FC, [
    caseOf(`${FC}-case1`, { mainError: failureOf("check-failed") }),
    caseOf(`${FC}-case2`, { mainError: interpreterRefused("interpreter-refused") })
  ])
  const prep = yield* cuts.open()
  yield* cuts.record({
    prepId: prep,
    capture: FC,
    flows: null,
    cases: [],
    refusals: [{
      caseId: `${FC}-auto`,
      reasons: ["source-call-incomplete", "egress-answer-not-captured"],
      callIds: ["auto@x"],
      detail: "cut short"
    }]
  })
})

const FAILED = [
  `${FV}-case1`,
  `${FV}-case2`,
  `${FA}-case1`,
  `${FA}-case2`,
  `${FB}-case1`,
  `${FB}-case2`,
  `${FC}-case1`,
  `${FC}-case2`
]
const RED = [...FAILED, `${FB}-cut4`, `${FC}-auto`]

const idsOf = (rows: ReadonlyArray<Calls.CallRow>): ReadonlyArray<string> => rows.map((row) => row.caseId)

const sortedGroups = (groups: ReadonlyArray<Facets.Group>): ReadonlyArray<Facets.Group> =>
  [...groups].sort((a, b) => (JSON.stringify(a) < JSON.stringify(b) ? -1 : 1))

describe.skipIf(!served)("Calls.queue", () => {
  beforeAll(() => withStore(seedFilters), 60_000)

  /** The family's queue under a query, `filt` narrowing to it unless the query names its own capture. */
  const queue = (query: Facets.Query, limit = 0) =>
    withStore(Effect.gen(function*() {
      const calls = yield* Calls.Service
      return yield* calls.queue(LANE, { capture: Q, ...query }, limit)
    }))

  it("defaults to failed: the SUT's failing calls, no refusal, nothing green or skipped", async () => {
    const out = await queue({})
    expect(idsOf(out.rows)).toEqual(FAILED)
    expect(out.matched).toBe(8)
    expect(out.captures).toBe(4)
  })

  it("red is the failing cases and the refused calls together, cases first", async () => {
    expect(idsOf((await queue({ scope: "red" })).rows)).toEqual(RED)
  })

  it("reads each scope as its own population", async () => {
    expect((await queue({ scope: "all" })).rows).toHaveLength(12)
    expect(idsOf((await queue({ scope: "passed" })).rows)).toEqual([`${FA}-case3`])
    expect((await queue({ scope: "failed" })).rows).toHaveLength(8)
    expect(idsOf((await queue({ scope: "skipped" })).rows)).toEqual([`${FB}-case3`])
    expect(idsOf((await queue({ scope: "refused" })).rows)).toEqual([`${FB}-cut4`, `${FC}-auto`])
  })

  it("narrows to a capture by substring, case-insensitively, on the run's and the cut's captures alike", async () => {
    expect(idsOf((await queue({ capture: "filt231E" })).rows)).toEqual([`${FV}-case1`, `${FV}-case2`])
    expect((await queue({ capture: "filt_" })).rows).toHaveLength(6)
    expect((await queue({ scope: "red", capture: "filt_" })).rows).toHaveLength(8)
    expect(idsOf((await queue({ scope: "refused", capture: "FILT_C" })).rows)).toEqual([`${FC}-auto`])
    // A blank substring narrows nothing.
    expect((await queue({ scope: "all", capture: "" })).rows.length).toBeGreaterThanOrEqual(12)
  })

  it("reads the substring literally: `_` and `%` are characters, not wildcards", async () => {
    await withStore(Effect.gen(function*() {
      yield* run("like_a.pcap.gz", [caseOf("like_a.pcap.gz-case1", { mainError: DIED })])
      yield* run("likeza.pcap.gz", [caseOf("likeza.pcap.gz-case1", { mainError: DIED })])
      yield* run("like%a.pcap.gz", [caseOf("like%a.pcap.gz-case1", { mainError: DIED })])
    }))
    expect(idsOf((await queue({ capture: "like" })).rows)).toEqual([
      "like%a.pcap.gz-case1",
      "like_a.pcap.gz-case1",
      "likeza.pcap.gz-case1"
    ])
    expect(idsOf((await queue({ capture: "like_a" })).rows)).toEqual(["like_a.pcap.gz-case1"])
    expect(idsOf((await queue({ capture: "like%a" })).rows)).toEqual(["like%a.pcap.gz-case1"])
    expect((await queue({ capture: "like\\" })).rows).toHaveLength(0)
  })

  it("narrows to an exact rule, an exact expected message, an exact refusal", async () => {
    expect(idsOf((await queue({ rule: "rport-echo" })).rows)).toEqual([`${FA}-case1`, `${FB}-case1`])
    expect((await queue({ rule: "rport" })).rows).toHaveLength(0)
    expect(idsOf((await queue({ expected: "200 to INVITE" })).rows)).toEqual([`${FV}-case1`, `${FA}-case2`])
    // A call refused under several reasons answers to each of them; a refusal
    // chip narrows a scope that holds refusals, and the default holds none.
    expect(idsOf((await queue({ scope: "red", refusal: "egress-answer-not-captured" })).rows))
      .toEqual([`${FB}-cut4`, `${FC}-auto`])
    expect(idsOf((await queue({ scope: "red", refusal: "source-call-incomplete" })).rows)).toEqual([`${FC}-auto`])
    expect((await queue({ refusal: "egress-answer-not-captured" })).rows).toHaveLength(0)
    expect((await queue({ scope: "red", refusal: "egress-answer-not-captured", rule: "rport-echo" })).rows)
      .toHaveLength(0)
    // The rule key matches an RFC rule and a failure tag alike, and the interpreter's refusal.
    expect(idsOf((await queue({ rule: "check-failed" })).rows)).toEqual([`${FC}-case1`])
    expect(idsOf((await queue({ rule: "interpreter-refused" })).rows)).toEqual([`${FC}-case2`])
    // A refused row answers to no rule and no expected message.
    expect((await queue({ scope: "refused", rule: "egress-answer-not-captured" })).rows).toHaveLength(0)
  })

  it("applies the capture and the chip together", async () => {
    expect(idsOf((await queue({ capture: "filt_a", expected: "200 to INVITE" })).rows)).toEqual([`${FA}-case2`])
  })

  it("hides every row carrying an excluded value, any of its values answering", async () => {
    // The call refused under two reasons goes with either of them.
    expect(idsOf((await queue({ scope: "refused", exclude: { refusal: ["source-call-incomplete"] } })).rows))
      .toEqual([`${FB}-cut4`])
    expect((await queue({ scope: "refused", exclude: { refusal: ["egress-answer-not-captured"] } })).rows)
      .toHaveLength(0)
    // Several values of one group, and several groups, hide together.
    expect(idsOf((await queue({
      exclude: {
        rule: ["rport-echo", "check-failed"],
        expected: ["200 to INVITE"],
        refusal: ["egress-answer-not-captured"]
      }
    })).rows)).toEqual([`${FV}-case2`, `${FB}-case2`, `${FC}-case2`])
    // An empty exclusion list hides nothing; a row with no value in the group is never hidden.
    expect((await queue({ exclude: { rule: [], expected: [], refusal: [], outcome: [] } })).rows).toHaveLength(8)
    expect((await queue({ exclude: { rule: ["nothing-carries-this"] } })).rows).toHaveLength(8)
  })

  it("narrows to an outcome: the row kinds the queue derives", async () => {
    expect(idsOf((await queue({ scope: "all", outcome: "refused" })).rows)).toEqual([`${FB}-cut4`, `${FC}-auto`])
    expect(idsOf((await queue({ scope: "all", outcome: "passed" })).rows)).toEqual([`${FA}-case3`])
    expect(idsOf((await queue({ scope: "all", outcome: "skipped" })).rows)).toEqual([`${FB}-case3`])
    expect((await queue({ scope: "all", outcome: "failed" })).rows).toHaveLength(8)
    // The outcome joins the scope: a passed call is not on the failed queue.
    expect((await queue({ outcome: "passed" })).rows).toHaveLength(0)
  })

  it("excludes an outcome: the runs stay when the refused calls go", async () => {
    const runs = (await queue({ scope: "red", exclude: { outcome: ["refused"] } })).rows
    expect(runs).toHaveLength(8)
    expect(runs.every((row) => row.kind === "case")).toBe(true)
    expect((await queue({ scope: "all", exclude: { outcome: ["refused", "passed", "skipped"] } })).rows)
      .toHaveLength(8)
  })

  it("applies an include and an exclude at once", async () => {
    expect(idsOf((await queue({ rule: "rport-echo", exclude: { expected: ["BYE"], outcome: ["refused"] } })).rows))
      .toEqual([`${FA}-case1`, `${FB}-case1`])
    expect(idsOf(
      (await queue({ scope: "red", outcome: "refused", exclude: { refusal: ["source-call-incomplete"] } })).rows
    )).toEqual([`${FB}-cut4`])
    // A value both included and excluded answers nothing: the exclusion stands.
    expect((await queue({ rule: "rport-echo", exclude: { rule: ["rport-echo"] } })).rows).toHaveLength(0)
  })

  it("serves the first `limit` rows of the order and counts the rest; 0 serves them all", async () => {
    const page = await queue({ scope: "red" }, 3)
    expect(idsOf(page.rows)).toEqual(RED.slice(0, 3))
    expect(page.matched).toBe(10)
    expect(page.captures).toBe(4)
    const whole = await queue({ scope: "red" }, 0)
    expect(whole.rows).toHaveLength(10)
    expect(whole.matched).toBe(10)
    // The count follows the includes; the captures follow the scope, the capture and the excludes.
    const narrowed = await queue({ scope: "red", rule: "rport-echo" }, 1)
    expect(narrowed.rows).toHaveLength(1)
    expect(narrowed.matched).toBe(2)
    expect(narrowed.captures).toBe(4)
  })

  it("counts the distinct captures the scope, the capture and the excludes keep", async () => {
    expect((await queue({ scope: "passed" })).captures).toBe(1)
    expect((await queue({ scope: "red", exclude: { refusal: ["source-call-incomplete"] } })).captures).toBe(4)
    // A capture whose every red call is hidden leaves the count: filt_b's are
    // its refused call, a rport-echo and a BYE.
    const emptied = await queue({
      scope: "red",
      capture: "filt_",
      exclude: { outcome: ["refused"], rule: ["rport-echo"], expected: ["BYE"] }
    })
    expect(emptied.captures).toBe(2)
    expect(idsOf(emptied.rows)).toEqual([`${FA}-case2`, `${FC}-case1`, `${FC}-case2`])
  })

  it("answers bad-body for every case carrying a failing body row, whatever its main error names", async () => {
    const SQ = "bodyq_a.pcap.gz"
    const badBody: MainError = { ...failureOf("bad-body"), kind: "bad-body", step: "s4" }
    await withStore(run(SQ, [
      caseOf(`${SQ}-case1`, { mainError: diedOf("s9", "BYE"), failedStep: "s9", bodyFailing: 2 }),
      caseOf(`${SQ}-case2`, { mainError: badBody, bodyFailing: 1 }),
      caseOf(`${SQ}-case3`, { mainError: diedOf("s9", "BYE"), failedStep: "s9" })
    ]))
    const out = await queue({ capture: "bodyq", rule: "bad-body" })
    expect(idsOf(out.rows)).toEqual([`${SQ}-case1`, `${SQ}-case2`])
    expect(out.rows.map((row) => row.bodyFailing)).toEqual([2, 1])
    expect(idsOf((await queue({ capture: "bodyq", exclude: { rule: ["bad-body"] } })).rows)).toEqual([`${SQ}-case3`])
    const pooled = await queue({ capture: "bodyq" })
    expect(Facets.facets(pooled.groups, { capture: "bodyq" }).failures).toEqual([{ value: "bad-body", count: 2 }])
    expect(sortedGroups(pooled.groups)).toEqual(sortedGroups(groupsOf(pooled.rows)))
  })

  it("hands back the pool's groups — the scope's and the capture's rows, whatever the chips — with their counts", async () => {
    const expected: ReadonlyArray<Facets.Group> = [
      { kind: "case", outcome: "failed", errorKind: "rfc", rule: "rport-echo", rfc: "RFC 3581 §4", expected: null, reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "rfc", rule: "rport-echo", rfc: null, expected: null, reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "died", rule: null, rfc: null, expected: "200 to INVITE", reasons: [], features: [], count: 2 },
      { kind: "case", outcome: "failed", errorKind: "died", rule: null, rfc: null, expected: "BYE", reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "rfc", rule: "no-200-after-cancel", rfc: "RFC 3261 §9.1", expected: null, reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "failure", rule: "check-failed", rfc: null, expected: null, reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "refused", rule: "interpreter-refused", rfc: null, expected: null, reasons: [], features: [], count: 1 },
      { kind: "refused", outcome: "refused", errorKind: "refused", rule: null, rfc: null, expected: null, reasons: ["egress-answer-not-captured"], features: [], count: 1 },
      { kind: "refused", outcome: "refused", errorKind: "refused", rule: null, rfc: null, expected: null, reasons: ["source-call-incomplete", "egress-answer-not-captured"], features: [], count: 1 }
    ]
    const red = await queue({ scope: "red" })
    expect(sortedGroups(red.groups)).toEqual(sortedGroups(expected))
    // The includes and the excludes narrow the page, never the groups.
    const chipped = await queue({ scope: "red", rule: "rport-echo", exclude: { refusal: ["source-call-incomplete"] } })
    expect(sortedGroups(chipped.groups)).toEqual(sortedGroups(expected))
    // The scope and the capture do: the passed pool is one green group.
    expect((await queue({ scope: "passed" })).groups).toEqual([
      { kind: "case", outcome: "passed", errorKind: null, rule: null, rfc: null, expected: null, reasons: [], features: [], count: 1 }
    ])
    expect(sortedGroups((await queue({ scope: "all", capture: "filt_b" })).groups)).toEqual(sortedGroups([
      { kind: "case", outcome: "failed", errorKind: "rfc", rule: "rport-echo", rfc: null, expected: null, reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "failed", errorKind: "died", rule: null, rfc: null, expected: "BYE", reasons: [], features: [], count: 1 },
      { kind: "case", outcome: "skipped", errorKind: null, rule: null, rfc: null, expected: null, reasons: [], features: [], count: 1 },
      { kind: "refused", outcome: "refused", errorKind: "refused", rule: null, rfc: null, expected: null, reasons: ["egress-answer-not-captured"], features: [], count: 1 }
    ]))
    // The groups are what the served rows say, aggregated: the two readings agree.
    expect(sortedGroups(red.groups)).toEqual(sortedGroups(groupsOf((await queue({ scope: "red" })).rows)))
    // The chips over them are the queue's chips.
    expect(Facets.facets(red.groups, { scope: "red" }).outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 8 }
    ])
  })

  it("keys a page through the cut's (capture, case_id, prep) index, one probe per served row, over 2 000 cases cut 4 times", () =>
    withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      const calls = yield* Calls.Service
      const cuts = yield* Cuts.Service
      const runs = yield* Runs.Service
      const capture = "capture_volume.pcap.gz"
      const seed = `${capture}-c0000`
      // Four preps of the same 2 000 cases, each keyed by its own prep, the
      // newest last: the page must carry prep 4's keys. One case document
      // serves them all; the statement is about the keys, not the documents.
      for (let at = 1; at <= 4; at++) {
        const prep = yield* cuts.open()
        yield* cuts.record({ prepId: prep, capture, flows: null, cases: [cutCase(seed)], refusals: [] })
        // Rows no frozen id names, past the guard: the statement is about the
        // page's reading, which a key per prep makes visible.
        yield* sql`ALTER TABLE cut_cases DISABLE TRIGGER cut_cases_frozen`
        yield* sql`
          INSERT INTO cut_cases (prep_id, capture, case_id, call_key, case_hash)
          SELECT ${prep}, ${capture}, ${capture} || '-c' || lpad(i::text, 4, '0'), 'p' || ${at} || '-K' || i || '@x',
                 (SELECT case_hash FROM cut_cases WHERE prep_id = ${prep} AND capture = ${capture} AND case_id = ${seed})
          FROM generate_series(1, 1999) AS i
        `
        yield* sql`ALTER TABLE cut_cases ENABLE TRIGGER cut_cases_frozen`
      }
      const opened = yield* runs.open({ capture, lane: LANE, mode: "record", sessionDir: `/w/${capture}`, identity: CURRENT })
      yield* runs.finish({ id: opened.id, status: "done", cases: [] })
      yield* sql`
        INSERT INTO case_results (run_id, case_id, passed, verdict, error_kind, error_rule)
        SELECT ${opened.id}, ${capture} || '-c' || lpad(i::text, 4, '0'), false, 'failed', 'failure', 'died'
        FROM generate_series(0, 1999) AS i
      `
      yield* sql.unsafe("ANALYZE runs; ANALYZE case_results; ANALYZE cut_cases; ANALYZE capture_cuts")

      const page = yield* calls.queue(LANE, { scope: "all", capture }, 300)
      expect(page.rows).toHaveLength(300)
      expect(page.matched).toBe(2000)
      expect(page.rows[0]).toMatchObject({ caseId: seed, callKey: `${seed}@key` })
      expect(page.rows[1]).toMatchObject({ caseId: `${capture}-c0001`, callKey: "p4-K1@x" })
      expect(page.rows[299]).toMatchObject({ caseId: `${capture}-c0299`, callKey: "p4-K299@x" })

      const plan = yield* calls.explain(LANE, { scope: "all", capture }, 300)
      console.log(["[calls] EXPLAIN (ANALYZE, BUFFERS) of Calls.queue over 2 000 cases cut 4 times, a page of 300:", ...plan].join("\n"))
      const at = plan.findIndex((line) => line.includes("Index Only Scan using cut_cases_capture_case_prep_idx on cut_cases"))
      expect(at, "the key probe reads the (capture, case_id, prep_id DESC) index").toBeGreaterThanOrEqual(0)
      const probe = plan[at]!
      const loops = Number(/loops=([\d.]+)/.exec(probe)?.[1])
      expect(loops, `one probe per served row: ${probe.trim()}`).toBeLessThanOrEqual(300)
      // Nothing under the probe filters rows away: the index answers the whole predicate.
      const depth = probe.search(/\S/)
      const under: Array<string> = []
      for (const line of plan.slice(at + 1)) {
        if (line.search(/\S/) <= depth) break
        under.push(line)
      }
      expect(under.filter((line) => line.includes("Rows Removed by Filter"))).toEqual([])
      expect(plan.join("\n")).not.toContain("Seq Scan on cut_cases")
    })), 120_000)
})
