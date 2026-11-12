/**
 * The LANE CATALOG: every lane a deployment drives, read from files. Each lane
 * is a `lane.v1` file (`lanes/<id>.toml`) naming its parts
 * (`targets/<kind>/<name>.toml`), loaded by {@link Lanes.load} over the rig
 * path of the pack, then the root the files are under. Each lane becomes its
 * view (`lane-view.ts`), then the lane the pack drives, through the pack's
 * {@link Reading} of the parts it names. The reading names the one simulated
 * lane, built in: its file is optional.
 *
 * A file the base, the view or the reading refuses refuses the whole catalog.
 * One setup serves a job, so the lanes of one job are of one group: they name
 * one file of each part kind the rig path declares shared, or all none.
 */
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as path from "node:path"
import * as Lane from "./lane.js"
import { type Hosted, hostedOf, type Simulated, simulatedBuiltIn, simulatedOf, type Timers, type View } from "./lane-view.js"
import * as Lanes from "./lanes.js"
import type { Refusal } from "./manifest-file.js"
import { TARGETS_DIR } from "./part.js"
import * as RigPath from "./rig-path.js"

/** Where the catalog reads its files: a root, its `lanes/` and its `targets/`. */
export interface Dirs {
  readonly root: string
  readonly lanes: string
  readonly targets: string
}

/** The directories under `root`. */
export const dirsUnder = (root: string): Dirs => ({ root, lanes: path.join(root, Lane.LANES_DIR), targets: path.join(root, TARGETS_DIR) })

/** The environment a part's `${VAR}` references are read from (`expand.ts`). */
export type Env = Readonly<Record<string, string | undefined>>

/** How a pack reads its lanes: the simulated lane `F` and each hosted lane `R` it drives, from its view and the parts it names, or why not (`E`). */
export interface Reading<F extends Simulated, R extends Hosted, E> {
  /** The pack's directory, the first root of the rig path. */
  readonly pack: string
  /** The built-in simulated lane's id. */
  readonly simulatedId: string
  /** Its in-process SUT's timers. */
  readonly simulatedTimers: Timers
  /** The simulated lane the pack drives, from its view; run on {@link simulatedBuiltIn} where no file states it. */
  readonly simulated: (lane: Simulated, env: Env) => Effect.Effect<F, E>
  /** A hosted lane the pack drives, from its view. */
  readonly hosted: (lane: Hosted, env: Env) => Effect.Effect<R, E>
}

/** Every lane, the simulated one first, then by each file's `order` and id, and the rig path they were loaded over. */
export interface Catalog<L extends View = View> {
  readonly dirs: Dirs
  readonly rig: RigPath.RigPath
  readonly lanes: ReadonlyArray<L>
}

/** The lane ids of `catalog`, in its order. */
export const ids = (catalog: Catalog): ReadonlyArray<string> => catalog.lanes.map((lane) => lane.id)

/** The lane `id` names in `catalog`. */
export const find = <L extends View>(catalog: Catalog<L>, id: string): L | undefined => catalog.lanes.find((lane) => lane.id === id)

/** The hosted lane `id` names in `catalog`. */
export const hosted = <L extends View>(catalog: Catalog<L>, id: string): Extract<L, { readonly kind: "hosted" }> | undefined => {
  const lane = find(catalog, id)
  return lane?.kind === "hosted" ? (lane as Extract<L, { readonly kind: "hosted" }>) : undefined
}

/** The catalog the files under `dirs` state as `reading` reads them, a part's variables read from `stated`, else the process's. */
export const load = <F extends Simulated, R extends Hosted, E>(
  reading: Reading<F, R, E>,
  dirs: Dirs,
  stated?: Env
): Effect.Effect<Catalog<F | R>, Refusal | E, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const env = stated ?? (yield* Effect.sync(() => ({ ...process.env })))
    const rig = yield* RigPath.load({ pack: reading.pack, workspace: dirs.root })
    let simulated: F | undefined
    const hosted: Array<R> = []
    for (const lane of yield* Lanes.load(rig, dirs.root)) {
      if (lane.id === reading.simulatedId) {
        simulated = yield* reading.simulated(yield* Effect.fromResult(simulatedOf(lane, reading.simulatedTimers)), env)
      } else {
        hosted.push(yield* reading.hosted(yield* Effect.fromResult(hostedOf(lane, reading.simulatedId)), env))
      }
    }
    const first = simulated ?? (yield* reading.simulated(simulatedBuiltIn(reading.simulatedId, reading.simulatedTimers), env))
    return { dirs, rig, lanes: [first, ...hosted] }
  })

/** `lanes` of `catalog` where they are of one group, else why not: the first shared kind they name apart, and what each names. */
export const shared = <L extends View>(catalog: Catalog, lanes: ReadonlyArray<L>): Result.Result<ReadonlyArray<L>, string> =>
  Result.mapBoth(Lanes.sharedOf(catalog.rig, lanes.map((lane) => lane.loaded)), {
    onFailure: (refusal) => refusal.reason,
    onSuccess: () => lanes
  })

/** The group a lane of `catalog` runs in: the part it names of each shared kind (`store one, no broker`). */
export const groupOf = (catalog: Catalog, lane: View): string =>
  Lanes.sharedNamesOf(catalog.rig, lane.loaded).map(([kind, name]) => (name === undefined ? `no ${kind}` : `${kind} ${name}`)).join(", ")

/** `lanes` of `catalog` by their group, each group and its lanes in the order first met. */
export const groupsOf = <L extends View>(catalog: Catalog, lanes: ReadonlyArray<L>): ReadonlyArray<ReadonlyArray<L>> => {
  const groups = new Map<string, Array<L>>()
  for (const lane of lanes) {
    const key = groupOf(catalog, lane)
    groups.set(key, [...(groups.get(key) ?? []), lane])
  }
  return [...groups.values()]
}

/** The groups of ids `groups` as a line: `a alone, or b + c`. */
export const groupsLine = (groups: ReadonlyArray<ReadonlyArray<string>>): string =>
  groups.map((ids) => (ids.length === 1 ? `${ids[0]} alone` : ids.join(" + "))).join(", or ")
