/**
 * The `calls-ended` collector: the B2BUA runner's active-call gauge read
 * `delay_s` after a cell's call ended, while the lane runs its next cells.
 * `collect` reads `b2bua_active_calls` at the metrics address of the lane's
 * `b2bua-runner` part and answers the cell's `calls-ended`: `pass` where the
 * worker holds no call, so it holds none of this cell's; `not-checkable`
 * where it holds some, which the lane's other cells may hold. The gauge is
 * the worker's, never one call's: it never fails a cell. It attaches to the
 * cell the reading (`json`) and where it read it (`link`).
 *
 * It needs no library: node runs it with its types stripped, it reads the
 * step's variables and `context.v1` itself, and answers in the lines of
 * ADR-0060 appendix §5 on stdout.
 */
import * as fs from "node:fs"
import * as path from "node:path"

/** The part kind naming the runner's metrics address. */
const PART = "b2bua-runner"

/** The verification it answers. */
const VERIFICATION = "calls-ended"

/** The gauge's sample line in the runner's Prometheus text. */
const GAUGE = /^b2bua_active_calls(?:\{\})?\s+(\d+)\s*$/m

/** What the collector reads of `context.v1`. */
interface Context {
  readonly lane?: { readonly parts: Readonly<Record<string, { readonly value: Readonly<Record<string, unknown>> }>> }
}

/** The variable `name`, or the step refused naming it. */
const variable = (name: string): string => {
  const value = process.env[name]
  if (value === undefined || value === "") throw new Error(`${name} is not set`)
  return value
}

/** The calls the runner publishing at `url` holds now. */
const activeCallsAt = async (url: string): Promise<number> => {
  const response = await fetch(url)
  if (!response.ok) throw new Error(`${url} answered ${response.status}`)
  const sample = GAUGE.exec(await response.text())
  if (sample === null) throw new Error(`${url} publishes no b2bua_active_calls`)
  return Number(sample[1])
}

/** The file under `SIPRIG_OUT` the reading is attached from. */
const READING = "reading.json"

const main = async (): Promise<void> => {
  if (variable("SIPRIG_STEP") !== "collect") return
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const metrics = context.lane?.parts[PART]?.value["metrics"]
  if (typeof metrics !== "string") throw new Error(`the lane's ${PART} part names no metrics address`)
  const url = `http://${metrics}/metrics`
  const active = await activeCallsAt(url)
  fs.writeFileSync(path.join(variable("SIPRIG_OUT"), READING), `${JSON.stringify({ b2bua_active_calls: active })}\n`)
  const attached = [
    { attachment: { name: VERIFICATION, kind: "json", title: "the worker's active calls after the call", file: READING } },
    { attachment: { name: `${VERIFICATION}-metrics`, kind: "link", title: "where the gauge was read", file: url } }
  ]
  for (const line of attached) process.stdout.write(`${JSON.stringify(line)}\n`)
  const line = active === 0
    ? { verification: VERIFICATION, answer: "pass", message: "the worker holds no call" }
    : {
      verification: VERIFICATION,
      answer: "not-checkable",
      message: `the worker holds ${active} call(s), which the lane's other cells may hold`
    }
  process.stdout.write(`${JSON.stringify(line)}\n`)
}

main().catch((error: unknown) => {
  process.stderr.write(`calls-ended: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
