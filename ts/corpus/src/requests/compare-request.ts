/**
 * `compare A B --request [--repeat N]`: the calls that moved between two run
 * sets — every class but `unchanged` — frozen as one replay request on the
 * lane they ran on, each call by the key its case id is frozen to, so the
 * request names calls and survives a re-cut. With `--repeat N` every job of
 * the request replays its cells N times over, and a comparison of the job's
 * runs classes a call whose passes disagree `unstable`. A moved call whose id
 * is frozen to no key is said so, not dropped silently.
 */
import { CaseIds, type Compare, Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import {
  DEFAULT_BATCH,
  DEFAULT_CLEANUP,
  DEFAULT_CONCURRENCY,
  DEFAULT_MEMORY_MAX,
  DEFAULT_STARTS_PER_SECOND
} from "../knobs.js"

/** What the request is asked to be. */
export interface Asked {
  /** The lane where the moved calls' after cells ran on several, or none. */
  readonly lane: string
  /** Passes per job, at least one. */
  readonly repeat: number
  readonly createdBy: string
}

export type Made =
  | {
    readonly _tag: "created"
    readonly id: number
    readonly cells: number
    /** The moved calls no frozen id keys, by `capture/case`. */
    readonly unkeyed: ReadonlyArray<string>
  }
  | { readonly _tag: "refused"; readonly why: string }

/** Why `--repeat` cannot be honoured: it states the passes of a request, so it needs `--request` and one pass at least. */
export const repeatRefusal = (request: boolean, repeat: number): string | null => {
  if (!Number.isInteger(repeat) || repeat < 1) return `--repeat ${repeat}: the passes per job are at least 1`
  if (repeat > 1 && !request) return "--repeat states the passes of the request --request makes; name --request"
  return null
}

/** How many passes a job of a request with these knobs replays its cells in. */
export const passes = (knobs: Requests.Knobs): number => Math.max(1, Math.floor(knobs.repeat ?? 1))

/** The comparison's moved calls frozen as one request, or why there is none to make. */
export const request = Effect.fn("CompareRequest.request")(function*(comparison: Compare.Comparison, asked: Asked) {
  const moved = comparison.moves.filter((move) => move.class !== "unchanged")
  const named = `compare ${comparison.before.selector} ${comparison.after.selector}`
  if (moved.length === 0) {
    return { _tag: "refused", why: `nothing moved between the two sides: no request to make (${named})` } satisfies Made
  }
  const caseIds = yield* CaseIds.Service
  const frozen = yield* caseIds.of([...new Set(moved.map((move) => move.capture))])
  const keyOf = (move: Compare.Move): string | undefined =>
    frozen.get(move.capture)?.find((one) => one.caseId === move.caseId)?.callKey
  const cells = moved.flatMap((move) => {
    const callKey = keyOf(move)
    return callKey === undefined ? [] : [{ capture: move.capture, callKey }]
  })
  const unkeyed = moved.filter((move) => keyOf(move) === undefined).map((move) => `${move.capture}/${move.caseId}`)
  if (cells.length === 0) {
    return { _tag: "refused", why: `no moved call has a frozen id to key it by: ${unkeyed.join(", ")}` } satisfies Made
  }
  const lanes = [...new Set(moved.map((move) => move.after.lane))]
  const requests = yield* Requests.Service
  const created = yield* requests.create({
    lane: lanes.length === 1 ? lanes[0]! : asked.lane,
    selection: { kind: "cells", file: named, whole: [] },
    knobs: {
      concurrency: DEFAULT_CONCURRENCY,
      batchSize: DEFAULT_BATCH,
      memoryMax: DEFAULT_MEMORY_MAX,
      startsPerSecond: DEFAULT_STARTS_PER_SECOND,
      cleanup: DEFAULT_CLEANUP,
      repeat: Math.max(1, Math.floor(asked.repeat))
    },
    createdBy: asked.createdBy,
    cells
  })
  return { _tag: "created", id: created.id, cells: created.cells, unkeyed } satisfies Made
})
