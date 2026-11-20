/**
 * One cell in a page, assembled: which gate decided it and why (a failing
 * deltas gate with the signatures no rule accepts), the verdict's failures,
 * the RFC audit's findings each on the ladder message it names, the lane's
 * facts, the HTTP exchanges in a line each (their answers read by the pack's
 * `routingLineOf`), the SUT's records, and the
 * call's logs (`cell-logs.ts`). What a reader otherwise filters out of the
 * cell's whole evidence; every message it names links to its read
 * (`cell-message.ts`); `cell-digest-text.ts` renders it.
 *
 * The gates are the three a cell answers to: the interpreter's verdict, the
 * RFC audit (a gating finding no document statement cancels fails the cell),
 * and the delta classification (a record or body row no rule accepts). A cell
 * fails where any of them does.
 */
import { messageRefOf } from "@sip/viewer-core/recording"
import { Effect } from "effect"
import { callLogsOf, type CallLogRead } from "./cell-logs.ts"
import { messageAt, replayOf } from "./cell-message.ts"
import { sutOf } from "./lane-roles.ts"
import { SitePack } from "./site-pack.ts"
import { TriageCatalog } from "./triage.ts"

/** The most signatures a failing deltas gate names. */
const DELTA_ROWS = 10

/** Why a finding with an offending message has no pointer to it. */
export const NO_POINTER = "no message pointer in this run's audit; rerun"

export interface Gate {
  readonly gate: "interpreter" | "verdict" | "rfc audit" | "deltas"
  readonly passed: boolean
  readonly line: string
}

export interface Finding {
  readonly rule: string
  /** gating | advisory | cancelled | information */
  readonly standing: string
  /** Who the audit holds responsible: `SUT`, or the scripted actor. */
  readonly charged: string
  readonly detail: string
  /** The ladder message the finding names, where it names one the ladder holds. */
  readonly at?: { readonly m: number; readonly step: string | undefined; readonly line: string; readonly href: string }
  /** Why it has no `at` though the rule pinpointed a message. */
  readonly note?: string
}

/** One delta signature no rule accepts, with how many rows carry it and at which steps. */
export interface DeltaRow {
  readonly signature: string
  readonly klass: string
  readonly rows: number
  readonly steps: ReadonlyArray<string>
}

export interface Digest {
  readonly capture: string
  readonly run: string
  readonly cell: string
  readonly lane: string | undefined
  readonly sut: string | undefined
  /** The outcome and the gate(s) deciding it, in a line. */
  readonly outcome: string
  readonly gates: ReadonlyArray<Gate>
  readonly failures: ReadonlyArray<string>
  readonly findings: ReadonlyArray<Finding>
  readonly laneFacts: Readonly<Record<string, unknown>>
  readonly routing: ReadonlyArray<string>
  readonly records: string | undefined
  /** The signatures a failing deltas gate failed on, most rows first; empty where it passed. */
  readonly deltaRows: ReadonlyArray<DeltaRow>
  readonly logs: ReadonlyArray<CallLogRead>
  readonly links: ReadonlyArray<{ readonly label: string; readonly href: string }>
}

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined

const list = (value: unknown): ReadonlyArray<unknown> => Array.isArray(value) ? value : []

const text = (value: unknown): string => typeof value === "string" ? value : JSON.stringify(value)

const segment = encodeURIComponent

/** A size as a reader weighs it. */
export const sizeOf = (bytes: number): string =>
  bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024 ? `${Math.round(bytes / 1024)} KB` : `${(bytes / 1048576).toFixed(1)} MB`

/** The three gates of a cell, read off its evidence. */
export const gatesOf = (detail: TriageCatalog.StudyCellDetail): ReadonlyArray<Gate> => {
  if (detail.error !== undefined) {
    return [{ gate: "interpreter", passed: false, line: `refused: ${detail.error.split("\n")[0] ?? ""}` }]
  }
  const verdict = record(detail.verdict)
  const status = typeof verdict?.["status"] === "string" ? verdict["status"] : undefined
  const failedStep = verdict?.["failed_step"]
  const failures = list(verdict?.["failures"])
  const findings = list(record(detail.rfc)?.["findings"]).flatMap((f) => {
    const one = record(f)
    return one === undefined ? [] : [one]
  })
  const standing = findings.filter((f) => f["gating"] === true && f["cancelled_by"] === undefined)
  const classification = record(detail.classification)
  const count = (field: string): number =>
    typeof classification?.[field] === "number" ? classification[field] as number : 0
  return [
    {
      gate: "verdict",
      passed: status === "ok" || status === "ok-negative",
      line: `status ${status ?? "none"}${typeof failedStep === "string" ? ` at ${failedStep}` : ""}` +
        (failures.length === 0 ? "" : ` · ${failures.length} failures`)
    },
    {
      gate: "rfc audit",
      passed: standing.length === 0,
      line: `${standing.length} gating finding${standing.length === 1 ? "" : "s"}` +
        (standing.length === 0 ? "" : `: ${standing.map((f) => text(f["rule"])).join(", ")}`) +
        ` · ${findings.length - standing.length} not gating`
    },
    classification === undefined
      ? { gate: "deltas", passed: true, line: "not classified" }
      : {
        gate: "deltas",
        passed: classification["passed"] !== false,
        line: `classification passed: ${text(classification["passed"])} · ${count("accepted")} accepted · ` +
          `${count("known_bug")} known-bug · ${count("unknown")} unknown · ${count("unlisted")} unlisted`
      }
  ]
}

/** The outcome in a line, naming the gate(s) that decided it. */
export const outcomeOf = (gates: ReadonlyArray<Gate>): string => {
  const failed = gates.filter((g) => !g.passed)
  if (failed.length === 0) return `passed: every gate passed (${gates.map((g) => g.gate).join(", ")})`
  const passed = gates.filter((g) => g.passed)
  return `failed by the ${failed.map((g) => `${g.gate} (${g.line})`).join(" and the ")}` +
    (passed.length === 0 ? "" : `; passed: ${passed.map((g) => g.gate).join(", ")}`)
}

/** A verdict failure in a line: its tag, then its other fields. */
const failureLine = (value: unknown): string => {
  const entry = record(value)
  if (entry === undefined) return text(value)
  const tag = entry["failure"] ?? entry["rule"] ?? "failure"
  return [text(tag), ...Object.entries(entry).filter(([k]) => k !== "failure").map(([k, v]) => `${k}: ${text(v)}`)]
    .join(" · ")
}

/** An HTTP exchange of a cell, as its evidence holds it. */
export interface Exchange {
  readonly ordinal: number
  readonly atMs: number
  readonly method: string
  readonly path: string
  readonly status?: number
  readonly error?: string
  readonly responseBody: unknown
}

/** An HTTP exchange in a line: what was asked, what came back, then the parts the pack reads off its answer body. */
export const routingLine = (exchange: Exchange, partsOf: SitePack.Pack["routingLineOf"]): string => {
  const head = `http#${exchange.ordinal} +${exchange.atMs}ms ${exchange.method} ${exchange.path} → ` +
    (exchange.status === undefined ? `no answer: ${exchange.error ?? "—"}` : `${exchange.status}`)
  return [head, ...partsOf(exchange.responseBody)].join(" · ")
}

/** The lane a cell ran on: the one asked, else its verdict's, else its name's last part (`<case>__<shape>__<lane>`). */
export const laneOf = (detail: TriageCatalog.StudyCellDetail, asked: string | undefined): string | undefined => {
  if (asked !== undefined) return asked
  const stated = record(detail.verdict)?.["lane"]
  if (typeof stated === "string") return stated
  const parts = detail.name.split("__")
  return parts.length < 3 ? undefined : parts[parts.length - 1]
}

/** The confrontation rows no rule accepts, folded by signature, most rows first. */
export const deltaRowsOf = (rows: ReadonlyArray<unknown>): ReadonlyArray<DeltaRow> => {
  const bySignature = new Map<string, { klass: string; rows: number; steps: Set<string> }>()
  for (const value of rows) {
    const row = record(value)
    const klass = row?.["class"]
    if (row === undefined || (klass !== "unknown" && klass !== "unlisted")) continue
    const signature = text(row["signature"] ?? row["kind"] ?? "row")
    const seen = bySignature.get(signature) ?? { klass, rows: 0, steps: new Set<string>() }
    seen.rows += 1
    if (typeof row["step"] === "string") seen.steps.add(row["step"])
    bySignature.set(signature, seen)
  }
  return [...bySignature.entries()]
    .map(([signature, one]) => ({ signature, klass: one.klass, rows: one.rows, steps: [...one.steps].slice(0, 5) }))
    .sort((a, b) => b.rows - a.rows)
    .slice(0, DELTA_ROWS)
}

/** The digest of one cell, its links absolute under `origin`. */
export const digestOf = (
  input: {
    readonly capture: string
    readonly run: string
    readonly lane: string | undefined
    readonly detail: TriageCatalog.StudyCellDetail
    readonly logs: ReadonlyArray<CallLogRead>
    /** The lanes whose SUT the digest names, and how it reads an exchange's answer. */
    readonly pack: Pick<SitePack.Pack, "lanes" | "simulatedSut" | "routingLineOf">
  },
  origin: string
): Digest => {
  const { capture, run, detail } = input
  const cellPath = `/api/triage/${segment(capture)}/runs/${segment(run)}/cells/${segment(detail.name)}`
  const messageHref = (query: string) => `${origin}${cellPath}/message?${query}`
  const verdict = record(detail.verdict)
  const lane = laneOf(detail, input.lane)
  const replay = replayOf(detail)
  const gates = gatesOf(detail)
  const findings = list(record(detail.rfc)?.["findings"]).flatMap((value): ReadonlyArray<Finding> => {
    const f = record(value)
    if (f === undefined) return []
    const ref = messageRefOf(f)
    const at = ref === undefined ? undefined : messageAt(replay, ref)
    const standing = f["gating"] === true && f["cancelled_by"] === undefined
      ? "gating"
      : f["cancelled_by"] !== undefined
      ? "cancelled"
      : f["advisory"] === true
      ? "advisory"
      : "information"
    return [{
      rule: text(f["rule"]),
      standing,
      charged: typeof f["actor"] === "string" ? `scripted ${f["actor"]}` : `SUT${f["charged"] === undefined ? "" : ` ${text(f["charged"])}`}`,
      detail: text(f["detail"]),
      ...(at === undefined ? {} : {
        at: { m: at.event.row, step: at.step, line: at.line, href: messageHref(`m=${at.event.row}`) }
      }),
      ...(typeof f["offending"] === "number" && ref === undefined ? { note: NO_POINTER } : {})
    }]
  })
  const order = { gating: 0, information: 1, cancelled: 2, advisory: 3 } as Record<string, number>
  const join = detail.join
  const evidence = JSON.stringify(detail).length
  return {
    capture,
    run,
    cell: detail.name,
    lane,
    sut: lane === undefined ? undefined : sutOf(input.pack, lane),
    outcome: outcomeOf(gates),
    gates,
    failures: list(verdict?.["failures"]).map(failureLine),
    findings: [...findings].sort((a, b) => (order[a.standing] ?? 9) - (order[b.standing] ?? 9)),
    laneFacts: record(verdict?.["lane_facts"]) ?? {},
    routing: (detail.http?.exchanges ?? []).map((exchange) => routingLine(exchange, input.pack.routingLineOf)),
    deltaRows: gates.some((g) => g.gate === "deltas" && !g.passed) ? deltaRowsOf(detail.confrontation) : [],
    records: join === null
      ? undefined
      : `Call-IDs ${join.callIds.join(", ")} · window ${join.from} → ${join.to} · held after the batch: ${detail.held.length}`,
    logs: input.logs,
    links: [
      {
        label: "ladder as text",
        href: `${origin}/api/triage/${segment(capture)}/runs/${segment(run)}/text?cell=${segment(detail.name)}` +
          (lane === undefined ? "" : `&lane=${segment(lane)}`)
      },
      { label: "one message: `message?m=<n>` or `message?step=<id>`", href: messageHref("m=0") },
      { label: `cell evidence, whole (JSON, ${sizeOf(evidence)})`, href: `${origin}/api/triage/${segment(capture)}/cells/${segment(run)}/${segment(detail.name)}` }
    ]
  }
}

/** The digest of the cell `run`/`cell` of `capture`. */
export const readDigest = Effect.fn("CellDigest.read")(function*(
  capture: string,
  run: string,
  cell: string,
  lane: string | undefined,
  origin: string
) {
  const triage = yield* TriageCatalog.Service
  const detail = yield* triage.cell(capture, run, cell)
  const logs = callLogsOf(detail)
  return digestOf({ capture, run, lane, detail, logs, pack: yield* SitePack.Service }, origin)
})
