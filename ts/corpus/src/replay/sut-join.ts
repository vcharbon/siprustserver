/**
 * What joins a real-lane cell to the SUT's own records, as its bundle states
 * it (`@sip/contracts` `Batch.SutJoin`): the Call-IDs the cell minted and the
 * wall-clock window it drove the SUT in.
 */
import { Batch } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as path from "node:path"
import type { Window } from "../log-window.js"

/** The bundle file the join is read from. */
export const FILE = Batch.SUT_JOIN_FILE

export interface SutJoin {
  /** The Call-ID of every call the cell placed, in document order. */
  readonly callIds: ReadonlyArray<string>
  readonly window: Window
}

/** The join of the cell at `cellDir`; `undefined` where it states none or not one this reads. */
export const read = (cellDir: string): Effect.Effect<SutJoin | undefined, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(path.join(cellDir, FILE)).pipe(Effect.orElseSucceed(() => undefined))
    if (text === undefined) return undefined
    const parsed = yield* Effect.try(() => JSON.parse(text) as unknown).pipe(Effect.orElseSucceed(() => undefined))
    return Option.match(Batch.decodeSutJoinOption(parsed), {
      onNone: () => undefined,
      onSome: (join): SutJoin => ({
        callIds: join.call_ids,
        window: { fromMs: join.started_at_ms, toMs: join.ended_at_ms }
      })
    })
  })

/** The joins of every cell directly under `runDir`, by cell directory name. */
export const ofRun = (runDir: string): Effect.Effect<ReadonlyMap<string, SutJoin>, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const joins = new Map<string, SutJoin>()
    for (const entry of yield* fs.readDirectory(runDir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))) {
      const join = yield* read(path.join(runDir, entry))
      if (join !== undefined) joins.set(entry, join)
    }
    return joins as ReadonlyMap<string, SutJoin>
  })
