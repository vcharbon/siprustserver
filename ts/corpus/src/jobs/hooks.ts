/**
 * The deployment's `sh -c` lines around a JOB (`Launcher.hooks`), run before
 * and after every job whoever launched it, their output kept whole under the
 * job (`<sessions>/_jobs/<id>/hooks/<name>.log`, streamed to disk as it
 * comes) so what the box was doing around a replay is readable months later.
 *
 * A hook NEVER fails its job — the job is the work, the hook only the record
 * around it — so a refusal, a spawn failure and a command that never exits are
 * all recorded as the log's exit line and the job reads on.
 *
 * The child inherits the job's environment plus the `HOOK_*` facts of the job
 * it speaks about: `HOOK_CAPTURE` (the captures, space-separated, while they
 * fit one environment string — a larger job states their count there),
 * `HOOK_CAPTURES_FILE` (every capture, one per line, under the hooks
 * directory), `HOOK_LANE`, `HOOK_STARTED_AT`, `HOOK_FINISHED_AT` (after),
 * `HOOK_RUN_DIR` (after, when the job produced exactly one run), `HOOK_JOB`
 * and `HOOK_JOB_DIR`.
 */
import { Logged } from "@sip/toolchain"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Stream from "effect/Stream"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"
import type { Launcher } from "../deployment/index.js"

/** One named shell line and the side of the job it runs on. */
export type Hook = Launcher.Hook

/** What a hook is told of the job it speaks about; an absent fact is an unset variable. */
export interface HookEnv {
  readonly job: number
  readonly jobDir: string
  readonly captures: ReadonlyArray<string>
  readonly lane: string
  readonly startedAt: string
  readonly finishedAt?: string
  readonly runDir?: string
}

/** One hook's record: its exit and the file holding every byte it wrote. */
export interface HookLog {
  readonly name: string
  readonly exit: number
  /** `<dir>/<name>.log`: the `exit N` line, then the output whole; absent where it could not be written. */
  readonly path: string
}

/** A command that never exits would hold the job forever. */
export const HOOK_TIMEOUT_MS = 120_000

/** Past this many characters, `HOOK_CAPTURE` names the count and the file instead of the list. */
export const CAPTURE_LIMIT = 65_536

/** The list itself while it fits, else its count and where the file is. */
export const captureLine = (captures: ReadonlyArray<string>): string => {
  const joined = captures.join(" ")
  return joined.length <= CAPTURE_LIMIT ? joined : `${captures.length} capture(s), see HOOK_CAPTURES_FILE`
}

const childEnv = (env: HookEnv, capturesFile: string): Record<string, string> => ({
  HOOK_JOB: String(env.job),
  HOOK_JOB_DIR: env.jobDir,
  HOOK_CAPTURE: captureLine(env.captures),
  HOOK_CAPTURES_FILE: capturesFile,
  HOOK_LANE: env.lane,
  HOOK_STARTED_AT: env.startedAt,
  ...(env.finishedAt === undefined ? {} : { HOOK_FINISHED_AT: env.finishedAt }),
  ...(env.runDir === undefined ? {} : { HOOK_RUN_DIR: env.runDir })
})

/** The suffix of a log while its output is still being written. */
const PARTIAL = ".part"

/**
 * `<name>.log` written whole once its command ended: the `exit N` line, every
 * byte the command wrote (streamed, never held), then the note of how it
 * ended where one is due. A record that cannot be written is lost; the job is
 * not.
 */
const finalise = (logPath: string, exit: number, note: string | undefined) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const partial = `${logPath}${PARTIAL}`
    yield* fs.writeFileString(logPath, `exit ${exit}\n`)
    if (yield* fs.exists(partial)) yield* fs.stream(partial).pipe(Stream.run(fs.sink(logPath, { flag: "a" })))
    if (note !== undefined) yield* fs.writeFileString(logPath, `\n${note}\n`, { flag: "a" })
    yield* fs.remove(partial, { force: true })
  }).pipe(Effect.ignore)

/** What a spawn failure says: the error under its cause, by its message. */
const said = (cause: unknown): string => {
  const error = Cause.isCause(cause) ? Cause.squash(cause) : cause
  return error instanceof Error ? error.message : String(error)
}

const runOne = (hook: Hook, env: HookEnv, capturesFile: string, dir: string, cwd: string) =>
  Effect.gen(function*() {
    const logPath = path.join(dir, `${hook.name}.log`)
    const ran = yield* Logged.run("sh", ["-c", hook.command], `${logPath}${PARTIAL}`, {
      cwd,
      env: childEnv(env, capturesFile),
      forceKillAfter: "1 second"
    }).pipe(
      Effect.map((exit) => ({ exit, note: undefined as string | undefined })),
      Effect.timeoutOrElse({
        duration: HOOK_TIMEOUT_MS,
        orElse: () => Effect.succeed({ exit: -1, note: `hook killed after ${HOOK_TIMEOUT_MS / 1000}s` })
      }),
      Effect.catch((error) => Effect.succeed({ exit: -1, note: `spawn failed: ${said(error.cause)}` }))
    )
    yield* finalise(logPath, ran.exit, ran.note)
    return { name: hook.name, exit: ran.exit, path: logPath } satisfies HookLog
  })

/** Every hook of this side, in order, one at a time; their logs written under `dir`, run in `cwd`. */
export const run = Effect.fn("Hooks.run")(function*(
  hooks: ReadonlyArray<Hook>,
  when: Hook["when"],
  env: HookEnv,
  dir: string,
  cwd: string
) {
  const wanted = hooks.filter((hook) => hook.when === when)
  if (wanted.length === 0) return [] as ReadonlyArray<HookLog>
  const fs = yield* FileSystem.FileSystem
  // The list as a file, whatever its size; an unwritable one is told to the hook by its absence.
  const capturesFile = path.join(dir, "captures.txt")
  yield* fs.makeDirectory(dir, { recursive: true }).pipe(
    Effect.andThen(fs.writeFileString(capturesFile, env.captures.map((capture) => `${capture}\n`).join(""))),
    Effect.ignore
  )
  const logs: Array<HookLog> = []
  for (const hook of wanted) logs.push(yield* runOne(hook, env, capturesFile, dir, cwd))
  return logs as ReadonlyArray<HookLog>
})

/** The platform a hook run reaches for. */
export type Requirements = FileSystem.FileSystem | ChildProcessSpawner.ChildProcessSpawner
