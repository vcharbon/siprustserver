/**
 * The main of a process that runs a job's operations (ADR-0060 appendix §3):
 * `NodeRuntime.runMain`'s contract, with SIGHUP among the signals that
 * interrupt the main fiber, so a closed terminal or a dropped session heals
 * the job's setup as SIGINT and SIGTERM do. A later signal is ignored: the
 * interruption already runs the heals.
 *
 * Its standard output and error outlive their reader: a write to a terminal
 * that hung up (EIO) or to a pipe whose reader closed (EPIPE) is dropped, and
 * the heal goes on. Any other write error still ends the process.
 */
import type * as Effect from "effect/Effect"
import * as Runtime from "effect/Runtime"

/** The signals that interrupt the main fiber. */
export const SIGNALS: ReadonlyArray<NodeJS.Signals> = ["SIGINT", "SIGTERM", "SIGHUP"]

/** The write error codes of an output whose reader is gone. */
const GONE = new Set(["EIO", "EPIPE", "ERR_STREAM_DESTROYED"])

/** Rethrows `error` unless it says the output's reader is gone. */
const unlessGone = (error: NodeJS.ErrnoException): void => {
  if (!GONE.has(error.code ?? "")) throw error
}

/**
 * Runs `effect` as the process's main fiber: the first of {@link SIGNALS}
 * interrupts it, its exit sets the process's exit code (`Runtime.defaultTeardown`
 * unless `teardown` is given: 130 for an interruption), and a write error of a
 * gone reader on standard output or error is dropped.
 */
export const runMain: {
  (
    options?: { readonly disableErrorReporting?: boolean | undefined; readonly teardown?: Runtime.Teardown | undefined }
  ): <E, A>(effect: Effect.Effect<A, E>) => void
  <E, A>(
    effect: Effect.Effect<A, E>,
    options?: { readonly disableErrorReporting?: boolean | undefined; readonly teardown?: Runtime.Teardown | undefined }
  ): void
} = Runtime.makeRunMain(({ fiber, teardown }) => {
  let signalled = false
  const onSignal = () => {
    signalled = true
    fiber.interruptUnsafe(fiber.id)
  }
  process.stdout.on("error", unlessGone)
  process.stderr.on("error", unlessGone)
  for (const signal of SIGNALS) process.on(signal, onSignal)
  fiber.addObserver((exit) => {
    for (const signal of SIGNALS) process.removeListener(signal, onSignal)
    teardown(exit, (code) => {
      if (signalled || code !== 0) process.exit(code)
    })
  })
})
