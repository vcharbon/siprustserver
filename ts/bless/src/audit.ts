/**
 * The BLESS AUDIT log: every place the post-run bless found something to
 * stand on where nothing needed blessing.
 *
 * A bless is only as good as the calls it does NOT fire on, so a passing run has
 * the bless premise put to it and what that finds is filed here.
 *
 * A finding is RULE-KEYED, never cell-keyed. The cell keeps its verdict
 * untouched: a bless with too wide a premise is a defect in the rule, and which
 * call happened to expose it is evidence, not a property of that call. So
 * nothing here is written into a cell directory or folded into a cell outcome —
 * one log per sweep, behind the {@link Sink} the sweep provides, aggregated by
 * rule. A pilot, a test or a one-off replay provides {@link Sink.silent}.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import * as path from "node:path"

/**
 * What the audit caught.
 *
 * - `bless-fires-on-a-green-cell` — the bless predicate itself holds on a run
 *   the interpreter passed. It cannot today (the premise needs at least one
 *   refused datagram and a passing verdict has none), and that is exactly why
 *   it is written down: it is the guard for the day someone widens the premise.
 * - `failure-shape-on-a-green-cell` — a shape record of a kind only a VERDICT
 *   FAILURE produces (`confront.ts::shapeProbes`) landed on a cell with no
 *   failures. The shape derivation and the verdict disagree, which is the
 *   mis-wiring that would let a bless answer a refusal nobody made.
 * - `rfc-inheritance-on-a-green-cell` — the RFC ruling (`./rfc-inherit.ts`)
 *   read a gating charge back to a peer on a cell the interpreter passed. It
 *   cannot today, a gating charge being what fails the cell, and is written
 *   down for the day the premise widens past the charges it was built for.
 * - `rfc-charge-on-a-green-cell` — a gating RFC finding the ruling did NOT read
 *   back to a peer, on a cell that passed anyway: the gate and the verdict
 *   disagree, and the cell greened over a charge nobody answered.
 */
export const Finding = Schema.Literals([
  "bless-fires-on-a-green-cell",
  "failure-shape-on-a-green-cell",
  "rfc-inheritance-on-a-green-cell",
  "rfc-charge-on-a-green-cell"
])
export type Finding = typeof Finding.Type

/**
 * The only shape kind a passing run may carry: the declared ACK-hold's
 * retransmit count, which `confront.ts::retransmissionProbes` derives from the
 * recording and never from a failure. Every other kind is failure-derived.
 */
export const HELD_FINAL_SHAPE = "retransmission"

/** The shape kind a record's signature names: `shape:<kind>:...`. */
export const shapeKindOf = (signature: string): string => signature.split(":")[1] ?? ""

/** One finding. `caseId` is the evidence that exposed the rule, not a verdict on it. */
export const AuditRecord = Schema.Struct({
  finding: Finding,
  lane: Schema.String,
  /** The named rules that classified the evidence; empty where none did. */
  rules: Schema.Array(Schema.String),
  /** The shape signatures the finding rests on. */
  signatures: Schema.Array(Schema.String),
  caseId: Schema.String,
  detail: Schema.String
})
export interface AuditRecord extends Schema.Schema.Type<typeof AuditRecord> {}

/** The log could not be reached: its directory not made, a line not appended or read. */
export class AuditLogUnreachable extends Schema.TaggedError<AuditLogUnreachable>()(
  "BlessAudit.AuditLogUnreachable",
  { op: Schema.Literals(["append", "read"]), path: Schema.String, cause: Schema.Defect() }
) {}

/** A line of the log is not an {@link AuditRecord}. */
export class AuditLogCorrupt extends Schema.TaggedError<AuditLogCorrupt>()(
  "BlessAudit.AuditLogCorrupt",
  { path: Schema.String, line: Schema.String, cause: Schema.Defect() }
) {}

/** Where a sweep's findings go, and how they are read back once the sweep is over. */
export interface Interface {
  /** File findings; a call with none appends nothing. */
  readonly append: (records: ReadonlyArray<AuditRecord>) => Effect.Effect<void, AuditLogUnreachable>
  /** Every finding filed so far, in filing order. */
  readonly read: () => Effect.Effect<ReadonlyArray<AuditRecord>, AuditLogUnreachable | AuditLogCorrupt>
}

/** The audit sink the reclassifier files into. */
export class Sink extends Context.Service<Sink, Interface>()("@sip/bless/BlessAudit/Sink") {
  /** Unwritten and empty: what a pilot, a test or a one-off replay provides. */
  static readonly silent: Layer.Layer<Sink> = Layer.succeed(
    Sink,
    Sink.of({ append: () => Effect.void, read: () => Effect.succeed([]) })
  )

  /** One NDJSON log at `file`, appended to by every cell of the sweep; unwritten where nothing was found. */
  static readonly file = (file: string): Layer.Layer<Sink, never, FileSystem.FileSystem> =>
    Layer.effect(
      Sink,
      Effect.gen(function* () {
        const fs = yield* FileSystem.FileSystem
        const unreachable = (op: "append" | "read") => (cause: unknown) =>
          new AuditLogUnreachable({ op, path: file, cause })

        const append = Effect.fn("BlessAudit.append")(function* (records: ReadonlyArray<AuditRecord>) {
          if (records.length === 0) return
          yield* fs.makeDirectory(path.dirname(file), { recursive: true }).pipe(
            Effect.mapError(unreachable("append"))
          )
          yield* fs
            .writeFileString(file, records.map((r) => `${JSON.stringify(r)}\n`).join(""), { flag: "a" })
            .pipe(Effect.mapError(unreachable("append")))
        })

        const read = (): Effect.Effect<ReadonlyArray<AuditRecord>, AuditLogUnreachable | AuditLogCorrupt> =>
          Effect.gen(function* () {
            const present = yield* fs.exists(file).pipe(Effect.mapError(unreachable("read")))
            if (!present) return []
            const text = yield* fs.readFileString(file).pipe(Effect.mapError(unreachable("read")))
            const out: Array<AuditRecord> = []
            for (const line of text.split("\n")) {
              if (line.trim() === "") continue
              const corrupt = (cause: unknown) => new AuditLogCorrupt({ path: file, line, cause })
              const parsed = yield* Effect.try({ try: () => JSON.parse(line) as unknown, catch: corrupt })
              out.push(yield* decodeRecord(parsed).pipe(Effect.mapError(corrupt)))
            }
            return out
          }).pipe(Effect.withSpan("BlessAudit.read"))

        return Sink.of({ append, read })
      })
    )
}

const decodeRecord = Schema.decodeUnknownEffect(AuditRecord)

/** How many cells cite one rule, and a handful of them by name. */
export interface ByRule {
  readonly rule: string
  readonly finding: Finding
  readonly count: number
  /** Up to a handful of cells, so the rule can be reproduced without the log. */
  readonly evidence: ReadonlyArray<string>
}

/**
 * How many cells one rule is cited by, most-cited first. A record naming no
 * rule is still a finding: the shape was classified by nothing, which is a
 * hole rather than a wide rule, and it aggregates under `(unnamed)`.
 */
export const byRule = (records: ReadonlyArray<AuditRecord>): ReadonlyArray<ByRule> => {
  const counts = new Map<string, { count: number; evidence: Array<string> }>()
  for (const record of records) {
    for (const rule of record.rules.length === 0 ? ["(unnamed)"] : record.rules) {
      const key = `${record.finding} ${rule}`
      const at = counts.get(key) ?? { count: 0, evidence: [] }
      at.count += 1
      if (at.evidence.length < 5) at.evidence.push(record.caseId)
      counts.set(key, at)
    }
  }
  return [...counts.entries()]
    .map(([key, at]) => {
      const [finding = "", rule = ""] = key.split(" ")
      return { rule, finding: finding as Finding, count: at.count, evidence: at.evidence }
    })
    .sort((a, b) => b.count - a.count || a.rule.localeCompare(b.rule))
}

/** The closing lines. Says nothing at all where the audit found nothing. */
export const report = (records: ReadonlyArray<AuditRecord>): ReadonlyArray<string> => {
  if (records.length === 0) return []
  const lines = [
    `[bless-audit] ${records.length} finding(s) — a rule fired where nothing needed it:`
  ]
  for (const row of byRule(records)) {
    lines.push(
      `   ! ${row.rule.padEnd(38)} ${row.finding} x${row.count} (e.g. ${row.evidence.join(", ")})`
    )
  }
  return lines
}
