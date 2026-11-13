/**
 * The `capture` collector: the B2BUA runner's packet capture over the job,
 * cut per failed cell. `open` starts the job's capture: it empties the
 * capture file the lane's `sip-capture` part names, the collector's lock
 * keeping every other job's capture off it, and the runner appends a section
 * to it each time a batch of the job stands it; `collect`, run only where a
 * cell of the job failed, reads the job's capture and attaches to each failed
 * cell the packets of its calls (`cut.ts`) as a `file` attachment, a pcapng
 * capture of its own, and a `table` of them, a row per packet.
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { blocksOf, cut, packetTableOf } from "./cut.ts"

/** The part kind naming the capture file. */
const PART = "sip-capture"

/** The name of the table of a failed cell's packets. */
const PACKETS = "capture-packets"

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
  const step = variable("SIPRIG_STEP")
  if (step === "open") return fs.existsSync(capture) ? fs.truncateSync(capture, 0) : undefined
  if (step !== "collect") return
  const out = variable("SIPRIG_OUT")
  const blocks = blocksOf(fs.existsSync(capture) ? fs.readFileSync(capture) : new Uint8Array())
  for (const cell of context.cells) {
    const file = `${cell.id}.pcapng`
    const table = `${cell.id}.packets.jsonl`
    const owned = cut(blocks, callIdsOf(cell))
    fs.writeFileSync(path.join(out, file), owned)
    fs.writeFileSync(path.join(out, table), packetTableOf(owned))
    const lines = [
      { attachment: { kind: "file", title: "the B2BUA runner's capture of the call", file }, cell: cell.id },
      { attachment: { name: PACKETS, kind: "table", title: "the packets of the call", file: table }, cell: cell.id }
    ]
    for (const line of lines) process.stdout.write(`${JSON.stringify(line)}\n`)
  }
}

try {
  main()
} catch (error) {
  process.stderr.write(`capture: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
}
