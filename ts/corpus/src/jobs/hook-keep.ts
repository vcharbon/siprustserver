/**
 * A job's hook logs kept with each run it made: `<run>/hooks/<name>.log` holds
 * the job's log (`hooks.ts`) cut to the window the run's cells drove the SUT
 * in, from the first cell's start to the last one's end (`sut-join.json`),
 * padded. A run whose cells state no window — every paused-clock run — keeps
 * none, and neither does a run no line of the log falls inside.
 *
 * Runs whose windows overlap share one cut, over the union of their windows:
 * one file, hard-linked into each run's `hooks/` (copied where no link can be
 * made). The job's log is read once, line by line, never held whole. The
 * job's own copy stays under the job; the run's copy outlives it.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import * as Stream from "effect/Stream"
import * as LogWindow from "../log-window.js"
import * as SutJoin from "../replay/sut-join.js"

/** Where a run keeps its hook logs, the directory the viewer reads them from. */
export const HOOKS_DIR = "hooks"

/** The window the cells of a run drove the SUT in, padded; `undefined` where none states one. */
export const runWindow = (joins: Iterable<SutJoin.SutJoin>): LogWindow.Window | undefined => {
  let window: LogWindow.Window | undefined
  for (const join of joins) {
    window = window === undefined
      ? join.window
      : { fromMs: Math.min(window.fromMs, join.window.fromMs), toMs: Math.max(window.toMs, join.window.toMs) }
  }
  return window === undefined ? undefined : LogWindow.padded(window)
}

/** Runs whose windows overlap, with the union of their windows. */
export interface Group {
  readonly runDirs: ReadonlyArray<string>
  readonly window: LogWindow.Window
}

/** `windows` merged where they overlap, in time order: no two groups overlap. */
export const groups = (windows: ReadonlyArray<readonly [string, LogWindow.Window]>): ReadonlyArray<Group> => {
  const sorted = [...windows].sort((a, b) => a[1].fromMs - b[1].fromMs)
  const merged: Array<{ runDirs: Array<string>; window: LogWindow.Window }> = []
  for (const [runDir, window] of sorted) {
    const last = merged.at(-1)
    if (last !== undefined && window.fromMs <= last.window.toMs) {
      last.runDirs.push(runDir)
      last.window = { fromMs: last.window.fromMs, toMs: Math.max(last.window.toMs, window.toMs) }
    } else {
      merged.push({ runDirs: [runDir], window })
    }
  }
  return merged
}

/** Links `file` into `runDir`'s hooks as `name`, a copy where no link can be made. */
const share = Effect.fn("HookKeep.share")(function*(file: string, runDir: string, name: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const target = path.join(runDir, HOOKS_DIR, name)
  yield* fs.makeDirectory(path.dirname(target), { recursive: true })
  yield* fs.remove(target, { force: true })
  yield* fs.link(file, target).pipe(Effect.catch(() => fs.copyFile(file, target)))
})

/**
 * Cuts the job log `name` under `hooksDir` into one file per group, written
 * under the group's first run and shared with the others; answers how many
 * runs keep it. A line before the log's first stamp falls in no group.
 */
const cutInto = (hooksDir: string, name: string, cut: ReadonlyArray<Group>) =>
  Effect.scoped(Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const path = yield* Path.Path
    const encoder = new TextEncoder()
    const fileOf = (index: number): string => path.join(cut[index]!.runDirs[0]!, HOOKS_DIR, name)
    const written = new Map<number, FileSystem.File>()
    let head: string | undefined
    let stamp = -Infinity
    let at = 0
    const take = (line: string) =>
      Effect.gen(function*() {
        if (head === undefined) {
          head = line
          return true
        }
        stamp = LogWindow.stampOf(line) ?? stamp
        while (at < cut.length && stamp > cut[at]!.window.toMs) at += 1
        if (at === cut.length) return false
        if (stamp < cut[at]!.window.fromMs) return true
        let file = written.get(at)
        if (file === undefined) {
          yield* fs.makeDirectory(path.dirname(fileOf(at)), { recursive: true })
          file = yield* fs.open(fileOf(at), { flag: "w" })
          yield* file.writeAll(encoder.encode(`${head}\n`))
          written.set(at, file)
        }
        yield* file.writeAll(encoder.encode(`${line}\n`))
        return true
      })
    yield* fs.stream(path.join(hooksDir, name)).pipe(
      Stream.decodeText(),
      Stream.splitLines,
      Stream.runForEachWhile(take)
    )
    let kept = 0
    for (const index of written.keys()) {
      for (const runDir of cut[index]!.runDirs.slice(1)) yield* share(fileOf(index), runDir, name)
      kept += cut[index]!.runDirs.length
    }
    return kept
  }))

/**
 * Cuts every log under the job's `hooksDir` into each run of `runDirs` its
 * cells' window reaches; answers how many run logs were kept. A log that
 * cannot be cut is a lost record, never a failed job.
 */
export const keep = Effect.fn("HookKeep.keep")(function*(hooksDir: string, runDirs: ReadonlyArray<string>) {
  const fs = yield* FileSystem.FileSystem
  const windows: Array<readonly [string, LogWindow.Window]> = []
  for (const runDir of new Set(runDirs)) {
    const window = runWindow((yield* SutJoin.ofRun(runDir)).values())
    if (window !== undefined) windows.push([runDir, window])
  }
  if (windows.length === 0) return 0
  const cut = groups(windows)
  const names = yield* fs.readDirectory(hooksDir).pipe(
    Effect.map((entries) => entries.filter((name) => name.endsWith(".log")).sort()),
    Effect.orElseSucceed((): ReadonlyArray<string> => [])
  )
  let kept = 0
  for (const name of names) {
    kept += yield* cutInto(hooksDir, name, cut).pipe(Effect.orElseSucceed(() => 0))
  }
  return kept
})
