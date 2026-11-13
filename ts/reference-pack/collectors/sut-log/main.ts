/**
 * The `sut-log` collector: the B2BUA runner's log, bracketed by its batch.
 * `open` keeps how long the log file the lane's `b2bua-runner` part names
 * already is; `collect` reads what the batch appended after it, attaches it
 * whole to the batch and, cut to each cell (`cut.ts`), to each cell, as `log`
 * attachments. It answers no verification.
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout, so it starts in a few milliseconds.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { cut, entriesOf } from "./cut.ts"

/** The part kind naming the runner's log file. */
const PART = "b2bua-runner"

/** Where `open` keeps the log's length. */
const OFFSET = "offset"

/** What the collector reads of a cell in `context.v1`. */
interface Cell {
  readonly id: string
  readonly timeline: { readonly start: { readonly wall_ms: number }; readonly end: { readonly wall_ms: number } }
  readonly legs: ReadonlyArray<{ readonly call_id?: string }>
}

/** What the collector reads of `context.v1`. */
interface Context {
  readonly cells: ReadonlyArray<Cell>
  readonly lane?: { readonly parts: Readonly<Record<string, { readonly value: Readonly<Record<string, unknown>> }>> }
}

/** The variable `name`, or the step refused naming it. */
const variable = (name: string): string => {
  const value = process.env[name]
  if (value === undefined || value === "") throw new Error(`${name} is not set`)
  return value
}

/** How long the file at `file` is, 0 where it does not exist yet. */
const lengthOf = (file: string): number => (fs.existsSync(file) ? fs.statSync(file).size : 0)

/** What was appended to `file` after its first `from` bytes. */
const appendedTo = (file: string, from: number): string =>
  fs.existsSync(file) ? fs.readFileSync(file).subarray(from).toString("utf8") : ""

/** `content` written at `file` under `out`, and the line attaching it to `cell` (the scope where absent). */
const attach = (out: string, file: string, title: string, content: string, cell?: string): void => {
  fs.writeFileSync(path.join(out, file), content)
  const line = { attachment: { kind: "log", title, file }, ...(cell === undefined ? {} : { cell }) }
  process.stdout.write(`${JSON.stringify(line)}\n`)
}

const main = (): void => {
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const log = context.lane?.parts[PART]?.value["log"]
  if (typeof log !== "string") throw new Error(`the lane's ${PART} part names no log file`)
  const offset = path.join(variable("SIPRIG_STATE"), OFFSET)
  const step = variable("SIPRIG_STEP")
  if (step === "open") return fs.writeFileSync(offset, String(lengthOf(log)))
  if (step !== "collect") return
  const out = variable("SIPRIG_OUT")
  const bracketed = appendedTo(log, Number(fs.readFileSync(offset, "utf8")))
  attach(out, "batch.log", "the B2BUA runner's log over the batch", bracketed)
  const entries = entriesOf(bracketed)
  for (const cell of context.cells) {
    const window = {
      startMs: cell.timeline.start.wall_ms,
      endMs: cell.timeline.end.wall_ms,
      callIds: cell.legs.flatMap((leg) => (leg.call_id === undefined ? [] : [leg.call_id]))
    }
    attach(out, `${cell.id}.log`, "the B2BUA runner's log over the call", cut(entries, window), cell.id)
  }
}

try {
  main()
} catch (error) {
  process.stderr.write(`sut-log: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
}
