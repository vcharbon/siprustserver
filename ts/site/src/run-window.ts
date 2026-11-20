/**
 * The wall-clock window one run occupied, as its text export states it.
 *
 * The run row is the authority — the driver stamps it when it opens and closes
 * the run — and a run the database holds no row for is bounded by its own
 * bundle instead, oldest to newest file mtime. Both are answers; which one was
 * read travels with the window, because a mtime window is when the run's
 * evidence was WRITTEN, which includes whatever was written after it ended.
 */
import { Effect, Layer, Option } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { SqlClient } from "effect/unstable/sql/SqlClient"

export interface Window {
  readonly startedAt: string
  readonly finishedAt: string
  /** What the bounds were read from: the `runs` row, or the bundle's mtimes. */
  readonly source: string
}

export interface Interface {
  /**
   * The window of the run `runId` names, bounded by `runDir` where the
   * database cannot answer. A run whose row is still open ends now: a trace
   * over an unfinished run is a trace up to this instant.
   */
  readonly of: (runId: number | undefined, runDir: string) => Effect.Effect<Window>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/RunWindow") {}

interface RunRow {
  readonly started_at: Date
  readonly finished_at: Date | null
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const sql = yield* SqlClient

    const row = Effect.fn("RunWindow.row")(function*(runId: number) {
      const rows = yield* sql<RunRow>`
        SELECT started_at, finished_at FROM runs WHERE id = ${runId}
      `.pipe(Effect.orElseSucceed(() => [] as ReadonlyArray<RunRow>))
      return rows[0]
    })

    /** Every file mtime under a bundle, the directories themselves left out. */
    const stamps = Effect.fn("RunWindow.stamps")(function*(root: string) {
      const found: number[] = []
      const pending = [root]
      for (;;) {
        const dir = pending.pop()
        if (dir === undefined) break
        const entries = yield* fs.readDirectory(dir).pipe(
          Effect.orElseSucceed(() => [] as ReadonlyArray<string>)
        )
        for (const entry of entries) {
          const child = path.join(dir, entry)
          const info = yield* fs.stat(child).pipe(Effect.option)
          if (Option.isNone(info)) continue
          if (info.value.type === "Directory") pending.push(child)
          else {
            const mtime = Option.getOrUndefined(info.value.mtime)
            if (mtime !== undefined) found.push(mtime.getTime())
          }
        }
      }
      return found
    })

    const of = Effect.fn("RunWindow.of")(function*(runId: number | undefined, runDir: string) {
      const record = runId === undefined ? undefined : yield* row(runId)
      if (record !== undefined) {
        return {
          startedAt: record.started_at.toISOString(),
          finishedAt: (record.finished_at ?? new Date()).toISOString(),
          source: `runs row ${runId}`
        } satisfies Window
      }
      const found = yield* stamps(runDir)
      const now = Date.now()
      return {
        startedAt: new Date(found.length === 0 ? now : Math.min(...found)).toISOString(),
        finishedAt: new Date(found.length === 0 ? now : Math.max(...found)).toISOString(),
        source: found.length === 0 ? "neither a run row nor a file: this instant" : "bundle file mtimes"
      } satisfies Window
    })

    return Service.of({ of })
  })
)

export * as RunWindow from "./run-window.ts"
