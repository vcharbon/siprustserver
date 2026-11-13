/**
 * The `capture` collector: the B2BUA runner's packet capture over the job,
 * cut per failed cell. `open` starts the job's capture: it keeps in its state
 * directory where the capture file the lane's `sip-capture` part names ends,
 * the runner appending a section to it each time a batch stands it; `collect`,
 * run only where a cell of the job failed, reads what the job appended and
 * attaches to each failed cell the packets of its calls (`cut.ts`) as a
 * `file` attachment, a pcapng capture of its own.
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { blocksOf, cut } from "./cut.ts"

/** The part kind naming the capture file. */
const PART = "sip-capture"

/** Where `open` keeps the capture's length. */
const OFFSET = "offset"

/** What the collector reads of a cell in `context.v1`. */
interface Cell {
  readonly id: string
  readonly keys: Readonly<Record<string, ReadonlyArray<string>>>
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

/** The Call-IDs of `cell`'s calls, every leg's and the base's `call_id` key's. */
const callIdsOf = (cell: Cell): ReadonlyArray<string> => [
  ...cell.legs.flatMap((leg) => (leg.call_id === undefined ? [] : [leg.call_id])),
  ...(cell.keys["call_id"] ?? [])
]

const main = (): void => {
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const capture = context.lane?.parts[PART]?.value["file"]
  if (typeof capture !== "string") throw new Error(`the lane's ${PART} part names no file`)
  const offset = path.join(variable("SIPRIG_STATE"), OFFSET)
  const step = variable("SIPRIG_STEP")
  if (step === "open") return fs.writeFileSync(offset, String(fs.existsSync(capture) ? fs.statSync(capture).size : 0))
  if (step !== "collect") return
  const out = variable("SIPRIG_OUT")
  const appended = fs.existsSync(capture) ? fs.readFileSync(capture).subarray(Number(fs.readFileSync(offset, "utf8"))) : new Uint8Array()
  const blocks = blocksOf(appended)
  for (const cell of context.cells) {
    const file = `${cell.id}.pcapng`
    fs.writeFileSync(path.join(out, file), cut(blocks, callIdsOf(cell)))
    const line = { attachment: { kind: "file", title: "the B2BUA runner's capture of the call", file }, cell: cell.id }
    process.stdout.write(`${JSON.stringify(line)}\n`)
  }
}

try {
  main()
} catch (error) {
  process.stderr.write(`capture: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
}
