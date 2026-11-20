/**
 * The `calls-ended` collector: the B2BUA runner's live calls read `delay_s`
 * after a cell's call ended, while the lane runs its next cells. `collect`
 * lists them at the operator address of the lane's `b2bua-runner` part
 * (`GET /admin/calls`, each call by the Call-IDs of its legs) and answers the
 * cell's `calls-ended` per call: `pass` where the runner lists none of the
 * cell's Call-IDs, `fail` where it still lists one, naming it, and
 * `not-checkable` where the cell names no Call-ID. The calls of the lane's
 * other cells, in progress or not, change nothing. It attaches to
 * the cell what it read (`json`) and where it read it (`link`).
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout.
 */
import * as fs from "node:fs"
import * as path from "node:path"

/** The part kind naming the runner's operator address. */
const PART = "b2bua-runner"

/** The verification it answers. */
const VERIFICATION = "calls-ended"

/** What the collector reads of `context.v1`: its one cell's legs, and the lane's parts. */
interface Context {
  readonly cells: ReadonlyArray<{ readonly legs: ReadonlyArray<{ readonly call_id?: string }> }>
  readonly lane?: { readonly parts: Readonly<Record<string, { readonly value: Readonly<Record<string, unknown>> }>> }
}

/** One live call as the runner lists it. */
interface LiveCall {
  readonly call_ref: string
  readonly call_ids: ReadonlyArray<string>
}

/** The variable `name`, or the step refused naming it. */
const variable = (name: string): string => {
  const value = process.env[name]
  if (value === undefined || value === "") throw new Error(`${name} is not set`)
  return value
}

/** The calls the runner listing them at `url` serves now. */
const liveCallsAt = async (url: string): Promise<ReadonlyArray<LiveCall>> => {
  const response = await fetch(url)
  if (!response.ok) throw new Error(`${url} answered ${response.status}`)
  const listed = (await response.json()) as { readonly calls?: unknown }
  if (!Array.isArray(listed.calls)) throw new Error(`${url} lists no calls`)
  return listed.calls as ReadonlyArray<LiveCall>
}

/**
 * The cell's answer, its calls' Call-IDs `own` of which the runner still
 * serves `held`: `not-checkable` where the cell placed no call to look for.
 */
const answerOf = (own: ReadonlyArray<string>, held: ReadonlyArray<string>) => {
  if (own.length === 0) return { verification: VERIFICATION, answer: "not-checkable", message: "the cell names no Call-ID" }
  if (held.length > 0) return { verification: VERIFICATION, answer: "fail", message: `the runner still serves ${held.join(", ")}` }
  return { verification: VERIFICATION, answer: "pass", message: `the runner serves none of the cell's ${own.length} Call-ID(s)` }
}

/** The file under `SIPRIG_OUT` the reading is attached from. */
const READING = "reading.json"

const main = async (): Promise<void> => {
  if (variable("SIPRIG_STEP") !== "collect") return
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const admin = context.lane?.parts[PART]?.value["admin"]
  if (typeof admin !== "string") throw new Error(`the lane's ${PART} part names no admin address`)
  const url = `http://${admin}/admin/calls`
  const live = await liveCallsAt(url)
  const listed = new Set(live.flatMap((call) => call.call_ids))
  const own = context.cells.flatMap((cell) => cell.legs.flatMap((leg) => (leg.call_id === undefined ? [] : [leg.call_id])))
  const held = own.filter((id) => listed.has(id))
  fs.writeFileSync(path.join(variable("SIPRIG_OUT"), READING), `${JSON.stringify({ calls: live, held })}\n`)
  const attached = [
    { attachment: { name: VERIFICATION, kind: "json", title: "the runner's live calls after the call", file: READING } },
    { attachment: { name: `${VERIFICATION}-calls`, kind: "link", title: "where the live calls were read", file: url } }
  ]
  for (const line of attached) process.stdout.write(`${JSON.stringify(line)}\n`)
  process.stdout.write(`${JSON.stringify(answerOf(own, held))}\n`)
}

main().catch((error: unknown) => {
  process.stderr.write(`calls-ended: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
