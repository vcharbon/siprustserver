/**
 * Where a capture's runs sit under the session tree: `<stem>/run<id>`, and a
 * run SET ASIDE under `<stem>/aside/run<id>` (`@sip/store` `aside.ts`). A study
 * lists both, the aside ones marked so no default lands on them; a cell or a
 * trace named by its run's name is found in either.
 */
import { Aside } from "@sip/store"
import * as Effect from "effect/Effect"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"

/** What this deployment's aside runs are: the pack's keep command is the one writer. */
export const ASIDE_LABEL = "kept falsify run"

/** One run directory of a capture: its name (`run<id>`), where it is, and whether it is set aside. */
export interface RunDir {
  readonly name: string
  readonly dir: string
  readonly aside: boolean
}

const RUN = /^run(\d+)$/

/** The run id a run directory's name (`run<id>`) states; undefined for any other name. */
export const runIdOf = (name: string): number | undefined => {
  const id = RUN.exec(name)?.[1]
  return id === undefined ? undefined : Number(id)
}

/** The `run<id>` directories directly under `dir`, in id order. */
const runsUnder = (dir: string, aside: boolean) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const entries = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as string[]))
    const runs: Array<RunDir & { readonly id: number }> = []
    for (const entry of entries) {
      const id = RUN.exec(entry)?.[1]
      if (id === undefined) continue
      const at = path.join(dir, entry)
      const held = yield* fs.stat(at).pipe(
        Effect.map((info) => info.type === "Directory"),
        Effect.orElseSucceed(() => false)
      )
      if (held) runs.push({ name: entry, dir: at, aside, id: Number(id) })
    }
    return runs.sort((a, b) => a.id - b.id).map(({ id: _, ...run }): RunDir => run)
  })

/** A capture's runs in id order, then its aside runs in id order. */
export const runDirsOf = (sessionDir: string) =>
  Effect.gen(function*() {
    const path = yield* Path
    return [
      ...(yield* runsUnder(sessionDir, false)),
      ...(yield* runsUnder(path.join(sessionDir, Aside.DIR), true))
    ] as ReadonlyArray<RunDir>
  })

/** The directory of the run a study listed as `name`: under the capture's own, else set aside. */
export const runDirOf = (sessionDir: string, name: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const own = path.join(sessionDir, name)
    const held = yield* fs.exists(own).pipe(Effect.orElseSucceed(() => false))
    return held ? own : path.join(sessionDir, Aside.DIR, name)
  })
