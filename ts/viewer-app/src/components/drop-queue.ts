/**
 * What one DROP of captures queues (`DropCaptures.tsx`): its files as one
 * capture or one capture each, each bound to the anonymisation level the
 * picker shows when the files are dropped ("" = the workspace's default), so
 * a level picked later applies to later drops only.
 */
import type { AnonymiseLevel } from "../api.ts"

/** The level picker: "" names the workspace's `[anonymise] level`. */
export type Picked = AnonymiseLevel | ""

/** One capture a drop queued. */
export interface Queued<F> {
  readonly id: number
  /** Its files, in ring order for a set. */
  readonly files: readonly F[]
  /** The level it goes to the inbox at, as picked when it was dropped. */
  readonly level: Picked
}

/** The picker and the next capture id, before a drop. */
export interface Picking {
  readonly level: Picked
  readonly next: number
}

/**
 * A drop of `files`, in ring order, under `picking`: the captures it queues
 * (all of them one capture where `asSet`, else one each), each at the level
 * picked, and the picking after it.
 */
export const drop = <F>(
  picking: Picking,
  files: readonly F[],
  asSet: boolean
): { readonly queued: readonly Queued<F>[]; readonly picking: Picking } => {
  const groups = asSet ? [files] : files.map((file) => [file])
  const queued = groups.map((group, at) => ({ id: picking.next + at, files: group, level: picking.level }))
  return { queued, picking: { level: picking.level, next: picking.next + groups.length } }
}
