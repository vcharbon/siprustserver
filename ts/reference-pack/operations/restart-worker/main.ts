/**
 * The `restart-worker` operation: the B2BUA runner of the lane restarted.
 * `inject` stops the runner the file of the lane's `runner-pid` part names:
 * it sends it the `signal` of `[params]` and awaits its exit, the runner's
 * orchestrator standing the next one (on the reference lane the base, one per
 * batch). A pid file naming no running process has nothing stopped. One
 * naming a process while no runner answers `GET /admin/cordon` with its
 * state at the `admin` address of the lane's `b2bua-runner` part refuses the
 * inject, the process left unsignalled: a pid file outliving its runner may
 * name another process.
 *
 * The runner signalled is kept in the state directory until its exit is
 * seen: `heal` SIGKILLs one an interrupted inject left running, after the
 * same check, and `probe` answers recovered once it is gone, not yet while
 * it runs. A stop seen through leaves nothing for either to do, so neither
 * signals a pid the system may since have handed to another process.
 *
 * It needs no library: node runs it with its types stripped and it reads
 * the step's variables and `context.v1` itself.
 */
import * as fs from "node:fs"
import * as path from "node:path"

/** The part kind naming the runner's admin address. */
const RUNNER_PART = "b2bua-runner"

/** The part kind naming the file that names the runner's pid. */
const PID_PART = "runner-pid"

/** The probe's exit status for "not yet" (`EX_TEMPFAIL`). */
const NOT_YET = 75

/** How often a stopping runner is looked at, in ms. */
const POLL_MS = 20

/** How long the operator route is given to answer, in ms. */
const ANSWER_WITHIN_MS = 1000

/** What the operation reads of `context.v1`. */
interface Context {
  readonly params?: { readonly signal?: string }
  readonly lane?: { readonly parts: Readonly<Record<string, { readonly value: Readonly<Record<string, unknown>> }>> }
}

/** The variable `name`, or the step refused naming it. */
const variable = (name: string): string => {
  const value = process.env[name]
  if (value === undefined || value === "") throw new Error(`${name} is not set`)
  return value
}

/** The field `field` of the lane's `kind` part, or the step refused naming it. */
const partField = (context: Context, kind: string, field: string): string => {
  const value = context.lane?.parts[kind]?.value[field]
  if (typeof value !== "string") throw new Error(`the lane's ${kind} part names no ${field}`)
  return value
}

/** Whether the process `pid` runs. */
const running = (pid: number): boolean => {
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM"
  }
}

/** The pid `file` names, undefined where it names none. */
const pidIn = (file: string): number | undefined => {
  if (!fs.existsSync(file)) return undefined
  const pid = Number(fs.readFileSync(file, "utf8").trim())
  return Number.isInteger(pid) && pid > 0 ? pid : undefined
}

/** Whether a runner answers at `admin`: `GET /admin/cordon` answered 200 with its readiness `state`. */
const answersAt = async (admin: string): Promise<boolean> => {
  try {
    const response = await fetch(`http://${admin}/admin/cordon`, { signal: AbortSignal.timeout(ANSWER_WITHIN_MS) })
    if (response.status !== 200) return false
    const body = (await response.json()) as { readonly state?: unknown }
    return typeof body.state === "string"
  } catch {
    return false
  }
}

/** Resolves once `pid` may be signalled as the lane's runner; refuses where no runner answers at the lane's admin address. */
const vouched = async (context: Context, pid: number): Promise<void> => {
  const admin = partField(context, RUNNER_PART, "admin")
  if (!(await answersAt(admin))) {
    throw new Error(`the pid file names process ${pid}, but no runner answers at ${admin}: not signalled`)
  }
}

/** Resolves once the process `pid` no longer runs. */
const gone = async (pid: number): Promise<void> => {
  while (running(pid)) await new Promise((resolve) => setTimeout(resolve, POLL_MS))
}

/** Stops the runner the lane's pid file names, and says what it did. */
const inject = async (context: Context, signalled: string): Promise<string> => {
  const pid = pidIn(partField(context, PID_PART, "file"))
  if (pid === undefined || !running(pid)) return "no runner runs on the lane"
  await vouched(context, pid)
  const signal = context.params?.signal ?? "SIGTERM"
  fs.writeFileSync(signalled, String(pid))
  process.kill(pid, signal)
  await gone(pid)
  fs.rmSync(signalled)
  return `the runner ${pid} stopped by ${signal}`
}

/** SIGKILLs the runner an interrupted inject left running, and says what it did. */
const heal = async (context: Context, signalled: string): Promise<string> => {
  const pid = pidIn(signalled)
  if (pid === undefined) return "no stop left under way"
  if (running(pid)) {
    await vouched(context, pid)
    process.kill(pid, "SIGKILL")
  }
  await gone(pid)
  fs.rmSync(signalled)
  return `the runner ${pid} gone`
}

const main = async (): Promise<void> => {
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const signalled = path.join(variable("SIPRIG_STATE"), "signalled")
  const step = variable("SIPRIG_STEP")
  if (step === "inject") return void process.stderr.write(`restart-worker: ${await inject(context, signalled)}\n`)
  if (step === "heal") return void process.stderr.write(`restart-worker: ${await heal(context, signalled)}\n`)
  if (step !== "probe") return
  const pid = pidIn(signalled)
  if (pid !== undefined && running(pid)) process.exitCode = NOT_YET
}

main().catch((error: unknown) => {
  process.stderr.write(`restart-worker: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
