/**
 * The `restart-worker` operation: the B2BUA runner of the lane restarted.
 * `inject` stops the runner the file of the lane's `runner-pid` part names:
 * it sends it the `signal` of `[params]` and awaits its exit, the runner's
 * orchestrator standing the next one (on the reference lane the base, one per
 * batch). A pid file naming no running process has nothing stopped. One
 * naming a process the runner at the `admin` address of the lane's
 * `b2bua-runner` part does not answer for refuses the inject, the process
 * left unsignalled: `GET /admin/cordon` there must answer its state and that
 * pid, since a pid file outliving its runner may name another process.
 *
 * The runner signalled is kept in the state directory until its exit is
 * seen, then the runner stopped: `heal` SIGKILLs one an interrupted inject
 * left running, after the same check, and a stop seen through leaves it
 * nothing to do, so it never signals a pid the system may since have handed
 * to another process. An inject that saw its runner stop names its pid in
 * `SIPRIG_OUT/stopped`, the death the base claims as the step's (the manifest
 * states it `stops` the system).
 *
 * `probe` answers not yet while the runner signalled runs. As a case's step
 * (`context.v1` scope `cell`) the lane's orchestrator stands the next runner,
 * so it answers recovered only once a runner answers at the admin address as
 * a process other than the one stopped; as a job's setup, where each batch
 * stands its own runner later, once the one signalled is gone.
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

/** The file under `SIPRIG_OUT` an inject names the runner it stopped in (`@sip/rig` `Operation.STOPPED_FILE`). */
const STOPPED = "stopped"

/** The probe's exit status for "not yet" (`EX_TEMPFAIL`). */
const NOT_YET = 75

/** How often a stopping runner is looked at, in ms. */
const POLL_MS = 20

/** How long the operator route is given to answer, in ms. */
const ANSWER_WITHIN_MS = 1000

/** What the operation reads of `context.v1`. */
interface Context {
  readonly scope?: string
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

/**
 * The pid of the runner answering at `admin`: `GET /admin/cordon` answered
 * 200 with its readiness `state` and its `pid`. Undefined where no runner
 * answers so.
 */
const runnerAt = async (admin: string): Promise<number | undefined> => {
  try {
    const response = await fetch(`http://${admin}/admin/cordon`, { signal: AbortSignal.timeout(ANSWER_WITHIN_MS) })
    if (response.status !== 200) return undefined
    const body = (await response.json()) as { readonly state?: unknown; readonly pid?: unknown }
    return typeof body.state === "string" && Number.isInteger(body.pid) ? (body.pid as number) : undefined
  } catch {
    return undefined
  }
}

/**
 * Resolves once `pid` may be signalled as the lane's runner: the runner
 * answering at the lane's admin address is the process `pid`. Refuses where
 * no runner answers there, or where the one answering is another process.
 */
const vouched = async (context: Context, pid: number): Promise<void> => {
  const admin = partField(context, RUNNER_PART, "admin")
  const answering = await runnerAt(admin)
  if (answering === undefined) {
    throw new Error(`the pid file names process ${pid}, but no runner answers at ${admin}: not signalled`)
  }
  if (answering !== pid) {
    throw new Error(`the pid file names process ${pid}, but the runner at ${admin} is process ${answering}: not signalled`)
  }
}

/** Resolves once the process `pid` no longer runs. */
const gone = async (pid: number): Promise<void> => {
  while (running(pid)) await new Promise((resolve) => setTimeout(resolve, POLL_MS))
}

/** Where the step keeps the runner it signalled and the one it saw stopped. */
interface Files {
  readonly signalled: string
  readonly stopped: string
}

/** The runner `pid`, signalled, seen gone: no longer signalled, and kept as the one stopped. */
const stoppedAs = async (files: Files, pid: number): Promise<void> => {
  await gone(pid)
  fs.writeFileSync(files.stopped, String(pid))
  fs.rmSync(files.signalled)
}

/** Stops the runner the lane's pid file names, names it in `out` once it stopped, and says what it did. */
const inject = async (context: Context, files: Files, out: string): Promise<string> => {
  const pid = pidIn(partField(context, PID_PART, "file"))
  if (pid === undefined || !running(pid)) return "no runner runs on the lane"
  await vouched(context, pid)
  const signal = context.params?.signal ?? "SIGTERM"
  fs.writeFileSync(files.signalled, String(pid))
  process.kill(pid, signal)
  await stoppedAs(files, pid)
  fs.writeFileSync(path.join(out, STOPPED), `${pid}\n`)
  return `the runner ${pid} stopped by ${signal}`
}

/** SIGKILLs the runner an interrupted inject left running, and says what it did. */
const heal = async (context: Context, files: Files): Promise<string> => {
  const pid = pidIn(files.signalled)
  if (pid === undefined) return "no stop left under way"
  if (running(pid)) {
    await vouched(context, pid)
    process.kill(pid, "SIGKILL")
  }
  await stoppedAs(files, pid)
  return `the runner ${pid} gone`
}

/**
 * Whether the lab recovered: the runner signalled gone and, as a case's step,
 * a runner answering at the admin address that is not the one stopped.
 */
const recovered = async (context: Context, files: Files): Promise<boolean> => {
  const signalled = pidIn(files.signalled)
  if (signalled !== undefined && running(signalled)) return false
  if (context.scope !== "cell") return true
  const answering = await runnerAt(partField(context, RUNNER_PART, "admin"))
  return answering !== undefined && answering !== (signalled ?? pidIn(files.stopped))
}

const main = async (): Promise<void> => {
  const context = JSON.parse(fs.readFileSync(variable("SIPRIG_CONTEXT"), "utf8")) as Context
  const state = variable("SIPRIG_STATE")
  const files: Files = { signalled: path.join(state, "signalled"), stopped: path.join(state, "stopped") }
  const step = variable("SIPRIG_STEP")
  if (step === "inject") {
    return void process.stderr.write(`restart-worker: ${await inject(context, files, variable("SIPRIG_OUT"))}\n`)
  }
  if (step === "heal") return void process.stderr.write(`restart-worker: ${await heal(context, files)}\n`)
  if (step === "probe" && !(await recovered(context, files))) process.exitCode = NOT_YET
}

main().catch((error: unknown) => {
  process.stderr.write(`restart-worker: ${error instanceof Error ? error.message : String(error)}\n`)
  process.exitCode = 1
})
