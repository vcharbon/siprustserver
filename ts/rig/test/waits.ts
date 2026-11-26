/**
 * Waits on what a test's real processes do, with no deadline of their own:
 * the package's test timeout is their only bound, so a loaded box slows them
 * and never fails them. A wait that a process's end can cut short rejects as
 * soon as that process ended without the event, saying how it ended.
 */
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import type { ChildProcess } from "node:child_process"

/** How the process a wait depends on ended, or `undefined` while it runs. */
export type Ended = () => string | undefined

/** Resolves once `holds` does, checking every 20 ms. */
export const until = async (holds: () => boolean): Promise<void> => {
  while (!holds()) await new Promise((resolve) => setTimeout(resolve, 20))
}

/**
 * Follows `child`'s exit: `exited` resolves with its code and signal, and
 * `ended` names them once it came. Called in the tick that spawned `child`,
 * so no exit goes unseen.
 */
export const exitOf = (child: ChildProcess): {
  readonly exited: Promise<{ readonly code: number | null; readonly signal: NodeJS.Signals | null }>
  readonly ended: Ended
} => {
  let how: string | undefined
  const exited = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) =>
    child.once("exit", (code, signal) => {
      how = `exited with code ${code} (signal ${signal})`
      resolve({ code, signal })
    })
  )
  return { exited, ended: () => how }
}

/** Follows `fiber`'s end: names how it ended once it did. */
export const endOf = <A, E>(fiber: Fiber.Fiber<A, E>): Ended => {
  let how: string | undefined
  void Effect.runPromise(Fiber.await(fiber)).then((exit) => {
    how = Exit.isSuccess(exit) ? "succeeded" : `failed: ${Cause.pretty(exit.cause)}`
  })
  return () => how
}

/**
 * Resolves once `seen()` holds `phase`; rejects as soon as `ended` names an
 * end without it, with that end and the phases seen. `seen` reads what the
 * process wrote, which is whole once it ended.
 */
export const phaseSeen = async (seen: () => ReadonlyArray<string>, phase: string, ended: Ended): Promise<void> => {
  await until(() => seen().includes(phase) || ended() !== undefined)
  const phases = seen()
  if (!phases.includes(phase)) throw new Error(`their writer ${ended()} before "${phase}"; phases seen: ${JSON.stringify(phases)}`)
}
