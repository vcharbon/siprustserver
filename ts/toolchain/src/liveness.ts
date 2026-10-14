/**
 * Whether a recorded process still runs.
 *
 * A pid alone is reusable, so a record that carries the process's START TICKS
 * (`<pid> <ticks>`, as {@link Detached} writes it) is checked against `/proc`:
 * the same pid with other ticks is another process. A bare pid is answered by
 * `kill(pid, 0)`, and an answer the kernel refuses to give (EPERM) counts as
 * alive: a lock or a job is never broken on a guess.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"

/** Whether `pid` runs; `true` wherever the answer is unknowable. */
export const alive = (pid: number): Effect.Effect<boolean> =>
  Effect.sync(() => {
    try {
      process.kill(pid, 0)
      return true
    } catch (cause) {
      return (cause as NodeJS.ErrnoException).code === "EPERM"
    }
  })

/** Field 22 of `/proc/<pid>/stat`: the process's start ticks; undefined when gone. */
export const ticksOf = Effect.fn("Liveness.ticksOf")(function*(pid: number) {
  const fs = yield* FileSystem.FileSystem
  const stat = yield* fs.readFileString(`/proc/${pid}/stat`).pipe(Effect.orElseSucceed(() => undefined))
  if (stat === undefined) return undefined
  // The command name sits in parentheses and may hold spaces; the fields are
  // counted from after its closing one.
  return stat.slice(stat.lastIndexOf(")") + 2).split(" ")[22 - 3]
})

/** What a pid file records: the pid, and the start ticks where the writer knew them. */
export interface Recorded {
  readonly pid: number
  readonly ticks: string | undefined
}

/** The `<pid> [<ticks>]` line of `file`; undefined where there is none to read. */
export const recorded = Effect.fn("Liveness.recorded")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => undefined))
  if (text === undefined) return undefined
  const [pid, ticks] = text.trim().split(/\s+/)
  const number = Number(pid)
  if (!Number.isInteger(number) || number <= 0) return undefined
  return { pid: number, ticks: ticks === undefined || ticks === "" ? undefined : ticks } satisfies Recorded
})

/** Whether the recorded process runs: same pid AND same start ticks where ticks were recorded. */
export const recordedAlive = (record: Recorded): Effect.Effect<boolean, never, FileSystem.FileSystem> =>
  record.ticks === undefined ? alive(record.pid) : Effect.map(ticksOf(record.pid), (ticks) => ticks === record.ticks)

export * as Liveness from "./liveness.js"
