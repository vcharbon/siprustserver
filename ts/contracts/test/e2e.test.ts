/**
 * The e2e record mirrors, cross-checked against the JSON Schemas the Rust side
 * commits under `e2e/schemas`.
 *
 * The comparison is a KEY INVENTORY, not a full schema equality: what a mirror
 * loses first is a field, and a cheap alarm that fires the day a field arrives
 * or leaves is worth more than an exact-schema test nobody can keep green. The
 * records with no committed schema (`RunResult`, `CampaignIndex`, `SeqDoc`) are
 * pinned by their declaration-order emit instead.
 */
import * as Schema from "effect/Schema"
import { describe, expect, it } from "vitest"
import {
  allChecksPassed,
  campaignPassed,
  cellDirName,
  CheckOp,
  decodeCampaignIndexSync,
  decodeRunResultSync,
  emitCampaignIndex,
  emitRunResult
} from "../src/e2e.js"
import * as LoadRun from "../src/loadrun.js"
import { anomalyIsGating, decodeSeqDocSync, rowDelivered } from "../src/seq.js"
import { E2E_SCHEMAS, readJson } from "./fixtures.js"

type JsonSchemaNode = {
  properties?: Record<string, unknown>
  required?: Array<string>
  $defs?: Record<string, JsonSchemaNode>
}

const inventory = (node: JsonSchemaNode) => ({
  properties: Object.keys(node.properties ?? {}).sort(),
  required: (node.required ?? []).slice().sort()
})

const mine = (schema: Schema.Top) =>
  inventory(Schema.toJsonSchemaDocument(schema as never).schema as JsonSchemaNode)

describe("the load-run index against its committed schema", () => {
  const committed = readJson(E2E_SCHEMAS, "load-run-index.schema.json") as JsonSchemaNode
  const defs = committed.$defs ?? {}

  const pairs: Array<[string, JsonSchemaNode, Schema.Top]> = [
    ["LoadRunIndex", committed, LoadRun.LoadRunIndex],
    ["LoadRunMeta", defs.LoadRunMeta, LoadRun.LoadRunMeta],
    ["CountRow", defs.CountRow, LoadRun.CountRow],
    ["LatencyRow", defs.LatencyRow, LoadRun.LatencyRow],
    ["CheckpointRow", defs.CheckpointRow, LoadRun.CheckpointRow],
    ["CheckSummaryRow", defs.CheckSummaryRow, LoadRun.CheckSummaryRow],
    ["Canaries", defs.Canaries, LoadRun.Canaries],
    ["SampleGroup", defs.SampleGroup, LoadRun.SampleGroup]
  ]

  it.each(pairs)("%s states the same keys, and the same ones required", (_name, committedNode, schema) => {
    expect(mine(schema)).toEqual(inventory(committedNode))
  })
})

describe("the e2e check operator against its committed schema", () => {
  it("holds exactly the operators the check grammar defines", () => {
    const checkSet = readJson(E2E_SCHEMAS, "check-set.schema.json") as {
      $defs: { CheckOp: { oneOf: Array<{ const: string }> } }
    }
    const committed = checkSet.$defs.CheckOp.oneOf.map((arm) => arm.const).sort()
    expect([...CheckOp.literals].sort()).toEqual(committed)
  })
})

// --- The records with no committed schema ------------------------------------

const SEQ_DOC = {
  title: "basic call",
  description: null,
  passed: true,
  lanes: [
    { id: "alice", label: "alice (127.0.0.1:5060)", kind: "ua" },
    { id: "b1", label: "b1 (127.0.0.1:5091)", kind: "node", group: "127.0.0.1:5091" }
  ],
  rows: [
    {
      atMs: 0,
      seq: 1,
      from: "alice",
      to: "b1",
      label: "INVITE sip:bob@…",
      detail: "INVITE sip:bob@h SIP/2.0\r\n\r\n",
      conn: null,
      kind: { sip: { delivered: true } }
    },
    {
      atMs: 3,
      seq: 2,
      from: "b1",
      to: "b2",
      label: "Data[Create/bak]",
      detail: null,
      conn: ":40007",
      kind: { repl: { delivered: false } }
    },
    { atMs: 5, seq: 3, from: "b1", to: null, label: "crash b1", detail: null, conn: null, kind: "lifecycle" }
  ],
  anomalies: [
    {
      check: "rfc.cseqInDialogOrder",
      detail: "CSeq went backwards",
      lane: "b1",
      endpoint: "lb",
      advisory: false,
      rowSeqs: [1],
      ruleSourced: true
    },
    { check: "recorder.unbound", detail: "no receiver", lane: null }
  ],
  epochBaseMs: 1_700_000_000_000
}

const RUN_RESULT = {
  cell: { case: "basic", shape: "transparent", infra: "fake" },
  passed: true,
  checks: [
    { on: "alice.invite", field: "from.userInfo", op: "eq", expected: "0009001", actual: "0009001", passed: true, detail: "equal" },
    { on: "alice.invite", field: "(anchor)", op: "exists", passed: false, detail: "no anchor was tagged" }
  ],
  rfc: SEQ_DOC.anomalies,
  media: [{ agent: "alice", wav: "alice.received.wav", classify: "tone:200hz", rms: 0.42 }],
  seqDoc: SEQ_DOC,
  timings: { firstMs: 0, lastMs: 120, messages: 8 }
}

describe("a cell result", () => {
  const result = decodeRunResultSync(RUN_RESULT)

  it("names its own directory the way the executor does", () => {
    expect(cellDirName(result.cell)).toBe("basic__transparent__fake")
  })

  it("emits in DECLARATION order, not sorted — the e2e byte discipline", () => {
    const text = emitRunResult(result)
    expect(text).toBe(`${JSON.stringify(RUN_RESULT, undefined, 2)}\n`)
    expect(text.endsWith("\n")).toBe(true)
    expect(JSON.parse(text)).toEqual(RUN_RESULT)
  })

  it("reads the row planes and the anomaly severity", () => {
    const doc = decodeSeqDocSync(SEQ_DOC)
    expect(doc.rows.map((row) => rowDelivered(row.kind))).toEqual([true, false, undefined])
    expect(doc.anomalies.map(anomalyIsGating)).toEqual([true, false])
  })

  it("folds the checks half of the cell verdict", () => {
    expect(allChecksPassed(result.checks)).toBe(false)
    expect(allChecksPassed(result.checks.slice(0, 1))).toBe(true)
  })

  it("refuses an unknown field rather than dropping it", () => {
    expect(() => decodeRunResultSync({ ...RUN_RESULT, spare: 1 })).toThrow()
  })
})

const CAMPAIGN = {
  campaign: "smoke",
  ts: "2026-08-25T09-00-00",
  cells: [
    { cell: { case: "basic", shape: "transparent", infra: "fake" }, passed: true, dir: "basic__transparent__fake" },
    {
      cell: { case: "basic", shape: "transparent", infra: "kind" },
      passed: false,
      dir: "basic__transparent__kind",
      error: "the RFC hard gate panicked"
    }
  ]
}

describe("a campaign index", () => {
  const index = decodeCampaignIndexSync(CAMPAIGN)

  it("emits in declaration order and reads back identically", () => {
    expect(emitCampaignIndex(index)).toBe(`${JSON.stringify(CAMPAIGN, undefined, 2)}\n`)
  })

  it("passes only when every cell did", () => {
    expect(campaignPassed(index)).toBe(false)
  })
})

describe("a load-run index", () => {
  const INDEX = {
    meta: {
      startedMs: 1000,
      finishedMs: 61000,
      finished: true,
      target: "172.20.255.250:5060",
      cps: 20,
      durationSecs: 60,
      maxInFlight: 2000,
      egress: "transparent",
      profile: "endurance baseline"
    },
    counts: [
      { scenario: "basic_call", class: "ok", case: "", chaos: "clear", count: 1180, ok: true },
      { scenario: "reinvite", class: "timeout", case: "bob@connected", chaos: "near", count: 2, ok: false },
      { scenario: "reinvite", class: "check_fail", case: "alice.invite", chaos: "clear", count: 3, ok: false }
    ],
    latency: [{ scenario: "basic_call", n: 1180, meanMs: 12.5, p50Ms: 10, p90Ms: 25, p99Ms: 40, maxMs: 88 }],
    checkpoints: [{ scenario: "basic_call", checkpoint: "ringing", n: 1180, p50Ms: 3, p90Ms: 8, p99Ms: 15 }],
    checks: [{ scenario: "reinvite", passed: 7, failed: 3 }],
    canaries: { orphans: 0, shed: 4, drops: 11, ringingExpected: 1185, ringingReceived: 1184 },
    samples: [
      { scenario: "reinvite", class: "check_fail", case: "alice.invite", chaos: "clear", pages: ["callflows/0.html"] }
    ]
  }
  const index = LoadRun.decodeLoadRunIndexSync(INDEX)

  it("emits in declaration order and reads back identically", () => {
    expect(LoadRun.emitLoadRunIndex(index)).toBe(`${JSON.stringify(INDEX, undefined, 2)}\n`)
  })

  it("sums the buckets the way the triage view reads them", () => {
    expect(LoadRun.totalCalls(index)).toBe(1185)
    expect(LoadRun.failedCalls(index)).toBe(5)
    expect(LoadRun.clearFailures(index)).toBe(3)
    expect(LoadRun.ringingRatio(index.canaries)).toBeCloseTo(1184 / 1185, 9)
    expect(LoadRun.ringingRatio({ ...index.canaries, ringingExpected: 0 })).toBe(1)
  })

  it("defaults the un-refined case discriminator an older index omitted", () => {
    const older = LoadRun.decodeLoadRunIndexSync({
      ...INDEX,
      counts: [{ scenario: "basic_call", class: "ok", chaos: "clear", count: 1, ok: true }]
    })
    expect(older.counts[0].case).toBe("")
  })
})
