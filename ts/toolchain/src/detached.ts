/**
 * A command run DETACHED: its launcher returns once it runs, and the
 * launcher's own death — a shell, a server — kills nothing.
 *
 * Its record is a directory: `cmd` (the command line), `started` (UTC),
 * `log` (stdout and stderr, whole), `pid` (the SUPERVISOR's pid and its start
 * ticks, so a reused pid is never mistaken for it) and `exit` (the code, which
 * the supervisor writes the instant the command ends, renamed into place). The
 * supervisor leads its own session, so a TERM to its group reaches the whole
 * tree: the command takes the TERM itself, and the supervisor, which ignores
 * it, still records the exit.
 *
 * Linux: `setsid`, `bash` and `/proc`.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"
import * as Liveness from "./liveness.js"
import { runner } from "./spawn.js"

/** The detached command could not be launched; `detail` says why. */
export class NotLaunched extends Schema.TaggedError<NotLaunched>()("Toolchain.NotLaunched", {
  dir: Schema.String,
  detail: Schema.String
}) {
  override get message(): string {
    return `${this.dir}: ${this.detail}`
  }
}

/**
 * The supervisor: runs the command into the log and records its exit. A TERM
 * or an INT never ends it: before the command starts a no-op handler takes
 * them (the command still starts with the default disposition), and once it
 * runs they are ignored — the command, in its group, takes them itself — so
 * its wait is never cut short and the code it records is the command's own.
 * It writes its pid record only once the command runs.
 */
const SUPERVISOR = `
d="$1"; shift
trap ':' TERM INT
"$@" >> "$d/log" 2>&1 &
child=$!
trap '' TERM INT
echo "$$ $(awk '{ print $22 }' "/proc/$$/stat")" > "$d/pid.tmp" && mv "$d/pid.tmp" "$d/pid"
wait "$child"; code=$?
echo "$code" > "$d/exit.tmp" && mv "$d/exit.tmp" "$d/exit"
exit "$code"
`

/** How long a launch waits for its supervisor to write its record. */
const ARMED_MS = 30_000

/** The launcher: the supervisor in a session of its own, in the background; its pid on stdout. */
const LAUNCHER = `
setsid bash -c '${SUPERVISOR.replace(/'/g, "'\\''")}' _ "$@" < /dev/null > /dev/null 2>&1 &
echo "$!"
`

/** Where a launch runs and what it adds to the environment it inherits. */
export interface LaunchOptions {
  readonly cwd?: string
  readonly env?: Record<string, string>
}

/** The record a launch writes: the directory, the supervisor's pid, the log. */
export interface Launched {
  readonly dir: string
  readonly pid: number
  readonly log: string
}

/** The command line as the `cmd` file states it, one shell-quoted word each. */
const quoted = (argv: ReadonlyArray<string>): string =>
  argv.map((word) => (/^[\w@%+=:,./-]+$/.test(word) ? word : `'${word.replace(/'/g, "'\\''")}'`)).join(" ")

/**
 * `argv` launched detached with its record under `dir`, which must not hold
 * a record still running. Returns once the supervisor runs and its pid file
 * is written.
 */
export const launch = Effect.fn("Detached.launch")(function*(
  dir: string,
  argv: ReadonlyArray<string>,
  options: LaunchOptions = {}
) {
  const fs = yield* FileSystem.FileSystem
  const refused = (detail: string) => new NotLaunched({ dir, detail })
  const held = yield* Liveness.recorded(path.join(dir, "pid"))
  if (held !== undefined && (yield* Liveness.recordedAlive(held))) {
    return yield* refused(`its record still runs (pid ${held.pid})`)
  }
  yield* fs.remove(dir, { recursive: true, force: true }).pipe(Effect.mapError((error) => refused(error.message)))
  yield* fs.makeDirectory(dir, { recursive: true }).pipe(Effect.mapError((error) => refused(error.message)))
  const write = (file: string, text: string) =>
    fs.writeFileString(path.join(dir, file), text).pipe(Effect.mapError((error) => refused(error.message)))
  yield* write("cmd", `${quoted(argv)}\n`)
  yield* write("started", `${new Date().toISOString().replace(/\.\d{3}Z$/, "Z")}\n`)
  yield* write("log", "")
  const run = yield* runner
  const output = yield* run
    .all("bash", ["-c", LAUNCHER, "_", dir, ...argv], {
      ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
      ...(options.env === undefined ? {} : { env: options.env })
    })
    .pipe(Effect.mapError((error) => refused(String(error.cause))))
  const pid = Number(output.stdout.trim())
  if (output.exitCode !== 0 || !Number.isInteger(pid) || pid <= 0) {
    return yield* refused(`the launcher said no pid: ${output.stderr.trim() || output.stdout.trim()}`)
  }
  // Returned with the record whole: the supervisor writes its pid file once it
  // leads its own session, the command runs and a TERM can no longer end it,
  // so a stop the instant this returns reaches the job and its exit is recorded.
  const started = Date.now()
  for (;;) {
    const record = yield* Liveness.recorded(path.join(dir, "pid"))
    if (record !== undefined && record.pid === pid) break
    if ((yield* Liveness.ticksOf(pid)) === undefined && (yield* exitOf(dir)) === undefined) {
      return yield* refused("the supervisor ended before it wrote its record")
    }
    if (Date.now() - started > ARMED_MS) return yield* refused(`the supervisor wrote no record within ${ARMED_MS} ms`)
    yield* Effect.sleep(10)
  }
  return { dir, pid, log: path.join(dir, "log") } satisfies Launched
})

/** The exit code the supervisor recorded under `dir`; undefined where it wrote none. */
export const exitOf = Effect.fn("Detached.exitOf")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(path.join(dir, "exit")).pipe(Effect.orElseSucceed(() => undefined))
  const code = text === undefined ? Number.NaN : Number(text.trim())
  return Number.isInteger(code) ? code : undefined
})

/** Whether the supervisor recorded under `dir` still runs. */
export const running = Effect.fn("Detached.running")(function*(dir: string) {
  const record = yield* Liveness.recorded(path.join(dir, "pid"))
  return record === undefined ? false : yield* Liveness.recordedAlive(record)
})

/** SIGTERM to `pid`, or to the whole group it leads where `group` says so. */
export const terminate = (pid: number, group: boolean): Effect.Effect<void, NotLaunched> =>
  Effect.try({
    try: () => process.kill(group ? -pid : pid, "SIGTERM"),
    catch: (cause) =>
      new NotLaunched({ dir: String(pid), detail: cause instanceof Error ? cause.message : String(cause) })
  })

/** SIGTERM to a process group: the detached supervisor's whole tree. */
export const terminateGroup = (pid: number): Effect.Effect<void, NotLaunched> => terminate(pid, true)

/** The platform a launch reaches for. */
export type Requirements = FileSystem.FileSystem | ChildProcessSpawner.ChildProcessSpawner

export * as Detached from "./detached.js"
