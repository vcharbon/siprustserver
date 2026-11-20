/**
 * A campaign's universe: every ref a case file may port, read where the pack
 * states it. A full run's report states each ref no case file ports as a
 * skipped `not-ported` result of every lane; a universe that is absent where
 * it is read is said so rather than counted as none ported.
 */
import type * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"

/** Where a pack's universe is read and how a report names it absent. */
export interface Universe {
  /** Where the universe is read. */
  readonly where: Effect.Effect<string>
  /** Every ref of the universe at `where`; undefined where none is there. */
  readonly refs: (where: string) => Effect.Effect<ReadonlyArray<string> | undefined, never, FileSystem.FileSystem>
  /** What a report says of `where` holding no universe. */
  readonly absent: (where: string) => string
  /** Where in it the universe is looked for, as the console names it once beside {@link absent}. */
  readonly looked: string
}

/** The refs among `refs` that `ported` does not name, sorted. */
export const notPorted = (refs: ReadonlyArray<string>, ported: ReadonlyArray<string>): ReadonlyArray<string> => {
  const done = new Set(ported)
  return refs.filter((ref) => !done.has(ref)).sort()
}
