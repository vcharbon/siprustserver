/**
 * One executable of the rig path run as ADR-0060 appendix §2 states it: its
 * own directory as working directory, `SIPRIG_CONTEXT`, `SIPRIG_OUT` and
 * `SIPRIG_STATE` set, its stdout and stderr appended to one log file. It runs
 * in a process group of its own: a signal sent to the runner's group never
 * reaches it, and its timeout or the runner's interruption kills the whole
 * group. A process it leaves running after it exits is left alone: an
 * `inject` may leave one, and its `heal` ends it.
 */
import * as Effect from "effect/Effect"
import { spawn } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { VARIABLES } from "./context.js"

/** What an executable is run with. */
export interface Invocation {
  /** Its manifest's directory, its working directory. */
  readonly dir: string
  /** Its path relative to {@link Invocation.dir}, as the manifest states it. */
  readonly file: string
  /** The `context.v1` file it reads. */
  readonly context: string
  /** The directory it writes attachment files into. */
  readonly out: string
  /** The directory kept from `inject` to `heal`, or from `open` to `close`. */
  readonly state: string
  readonly timeoutMs: number
  /** The file its stdout and stderr are appended to. */
  readonly log: string
}

/** How an executable ended. */
export type Ended =
  | { readonly _tag: "Exited"; readonly code: number }
  | { readonly _tag: "Signalled"; readonly signal: string }
  | { readonly _tag: "TimedOut"; readonly afterMs: number }
  | { readonly _tag: "Unstarted"; readonly reason: string }

/** `ended` in words, for an event or a log line. */
export const describe = (ended: Ended): string => {
  switch (ended._tag) {
    case "Exited":
      return `exited ${ended.code}`
    case "Signalled":
      return `killed by ${ended.signal}`
    case "TimedOut":
      return `timed out after ${ended.afterMs} ms`
    case "Unstarted":
      return `did not start: ${ended.reason}`
  }
}

/** Whether `ended` is exit status 0: the executable did its work. */
export const succeeded = (ended: Ended): boolean => ended._tag === "Exited" && ended.code === 0

/** Kills the process group `pid` leads, where it still exists. */
const killGroup = (pid: number | undefined): void => {
  if (pid === undefined) return
  try {
    process.kill(-pid, "SIGKILL")
  } catch {
    return
  }
}

/** Runs `invocation` to its end; interrupted, it kills the executable's process group. */
export const run = (invocation: Invocation): Effect.Effect<Ended> =>
  Effect.callback<Ended>((resumeOnce) => {
    let resumed = false
    const resume = (effect: Effect.Effect<Ended>) => {
      if (resumed) return
      resumed = true
      resumeOnce(effect)
    }
    let log: number
    try {
      log = fs.openSync(invocation.log, "a")
    } catch (error) {
      resume(Effect.succeed({ _tag: "Unstarted", reason: `its log ${invocation.log}: ${(error as Error).message}` }))
      return
    }
    const child = spawn(path.resolve(invocation.dir, invocation.file), [], {
      cwd: invocation.dir,
      detached: true,
      stdio: ["ignore", log, log],
      env: {
        ...process.env,
        [VARIABLES.context]: invocation.context,
        [VARIABLES.out]: invocation.out,
        [VARIABLES.state]: invocation.state
      }
    })
    fs.closeSync(log)
    let timedOut = false
    const timer = setTimeout(() => {
      timedOut = true
      killGroup(child.pid)
    }, invocation.timeoutMs)
    child.once("error", (error) => {
      clearTimeout(timer)
      resume(Effect.succeed({ _tag: "Unstarted", reason: error.message }))
    })
    child.once("exit", (code, signal) => {
      clearTimeout(timer)
      resume(Effect.succeed(
        timedOut
          ? { _tag: "TimedOut", afterMs: invocation.timeoutMs }
          : code !== null
          ? { _tag: "Exited", code }
          : { _tag: "Signalled", signal: signal ?? "an unknown signal" }
      ))
    })
    return Effect.sync(() => {
      clearTimeout(timer)
      killGroup(child.pid)
    })
  })
