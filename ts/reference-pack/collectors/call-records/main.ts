/**
 * The `call-records` collector: the records the B2BUA runner writes of its
 * batch's calls, one JSON line each in the file the lane's `cdr-file` part
 * names. `open` empties the file, the collector's lock keeping every other
 * batch's collector off it, so it holds the batch's records alone; `collect`
 * waits until a record of each cell's caller Call-ID is in, `settle_s` at
 * most and never past its deadline, then answers each record keyed by its
 * a-leg's Call-ID, which the base joins on `call_id`.
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout.
 */
import * as fs from "node:fs"
import { setTimeout as sleep } from "node:timers/promises"

/** The part kind naming the record file. */
const PART = "cdr-file"

/** How often `collect` reads the file again while it waits. */
const POLL_MS = 50

/** What the collector reads of a cell in `context.v1`. */
interface Cell {
  readonly keys: Readonly<Record<string, ReadonlyArray<string>>>
}

/** What the collector reads of `context.v1`. */
interface Context {
  readonly cells: ReadonlyArray<Cell>
  readonly lane?: { readonly parts: Readonly<Record<string, { readonly value: Readonly<Record<string, unknown>> }>> }
  readonly params: Readonly<Record<string, unknown>>
  readonly deadline?: { readonly wall_ms: number }
}

/** A record as the runner writes one, the field the collector reads. */
interface CallRecord {
  readonly a_leg?: { readonly call_id?: unknown }
}

/** The variable `name`, or the step refused naming it. */
const variable = (name: string): string => {
  const value = process.env[name]
  if (value === undefined || value === "") throw new Error(`${name} is not set`)
  return value
}

/** The records `file` holds, a line that does not parse yet skipped. */
const recordsIn = (file: string): ReadonlyArray<CallRecord> =>
  !fs.existsSync(file) ? [] : fs.readFileSync(file, "utf8").split("\n").flatMap((line) => {
    try {
      return line.trim() === "" ? [] : [JSON.parse(line) as CallRecord]
    } catch {
      return []
    }
  })

/** The a-leg Call-ID of `record`, empty where it states none. */
const keyOf = (record: CallRecord): string => (typeof record.a_leg?.call_id === "string" ? record.a_leg.call_id : "")

/** Whether `records` hold one of every cell of `cells`. */
const coversAll = (records: ReadonlyArray<CallRecord>, cells: ReadonlyArray<Cell>): boolean => {
  const keys = new Set(records.map(keyOf))
  return cells.every((cell) => (cell.keys["call_id"] ?? []).some((callId) => keys.has(callId)))
}

/** The records of `file` once they hold one of every cell of `context`, or once its wait is over. */
const settled = async (file: string, context: Context): Promise<ReadonlyArray<CallRecord>> => {
  const settle = typeof context.params["settle_s"] === "number" ? context.params["settle_s"] * 1000 : 0
  const until = Math.min(Date.now() + settle, context.deadline?.wall_ms ?? Infinity)
  for (;;) {
    const records = recordsIn(file)
    if (coversAll(records, context.cells) || Date.now() + POLL_MS >= until) return records
    await sleep(POLL_MS)
  }
}

const main = async (): Promise<void> => {
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const file = context.lane?.parts[PART]?.value["file"]
  if (typeof file !== "string") throw new Error(`the lane's ${PART} part names no file`)
  const step = variable("SIPRIG_STEP")
  if (step === "open") return fs.existsSync(file) ? fs.truncateSync(file, 0) : undefined
  if (step !== "collect") return
  for (const record of await settled(file, context)) {
    process.stdout.write(`${JSON.stringify({ record, key: keyOf(record) })}\n`)
  }
}

main().catch((error: unknown) => {
  process.stderr.write(`call-records: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
