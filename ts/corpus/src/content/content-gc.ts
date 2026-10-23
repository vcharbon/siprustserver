/**
 * Garbage collection over `_content/`, the materialiser's cache.
 *
 * Nothing here is evidence. Every byte under `_content/` rebuilds from the
 * `cases`, `case_documents`, `case_assets`, `ledger_rows` and
 * `flows_documents` rows, so the worst a wrong answer costs is a
 * re-materialisation — EXCEPT for the one thing that would be a real fault:
 * removing a directory a running cell has already resolved a path to.
 * `Content.materialise` returns an existing hash without reading a row, and the
 * cell reads the files under it later, so an entry deleted in that window
 * fails a run for no reason at all.
 *
 * Two things make that window unreachable, in the order they bite:
 *
 * 1. **What the store cites.** An entry is a candidate only when no reading at
 *    or above the lowest live prep names it: the standing prep, floored by the
 *    preps open runs still pin. A command replaying an old pin holds its rows
 *    open, so its ceiling is live for as long as it runs, and the caller's
 *    reading names every hash a cut from that floor up can name.
 * 2. **A grace period on mtime.** An entry younger than {@link GRACE_HOURS} is
 *    never collected. `materialise` publishes by rename, so an entry that
 *    appears between the query and the removal is younger than the pass itself,
 *    and one a command is about to read that it materialised during its own
 *    plan phase is younger than the plan.
 *
 * There is no third belt. Nothing under `_content/` is written except by a
 * rename of finished content, so a pass that runs beside a live replay can see
 * a half-written entry only as a `.writing-` scratch, which the grace period
 * covers.
 *
 * The pass declines to rule where the cut store holds no cut at all: an empty
 * `capture_cuts` is a wiped database, not an empty cache, and every entry would
 * read as collectable while none of it could be rebuilt.
 *
 * `_ledgers/` entries are keyed by the SET of cases a batch ran together and
 * nothing in the store names them, so they take the only rule that is sound for
 * a directory the database cannot vouch for: collected when they are past the
 * grace period AND no run is open at all, since a merged side-row directory can
 * only be in use by a run that has not finished.
 */
import type { Cuts } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as path from "node:path"
import { CONTENT, FLOWS, LEDGERS } from "./content.js"

/**
 * How long an entry is untouchable after it is published. Comfortably longer
 * than the plan phase of the largest batch, which is the window between a
 * command resolving a case directory and its run rows going open.
 */
export const GRACE_HOURS = 6

/** A hash directory or flows file: 64 lowercase hex, as the hashes are written. */
const HASH = /^[0-9a-f]{64}$/

/** The store cites nothing, so nothing may be collected: a wiped database, not an empty cache. */
export const NO_CUT = "the cut store holds no cut — the cache is unreconstructible, so nothing goes"

export interface Collected {
  readonly cases: ReadonlyArray<string>
  readonly flows: ReadonlyArray<string>
  readonly ledgers: ReadonlyArray<string>
  /** Half-written materialisations a crashed process left behind. */
  readonly scratch: ReadonlyArray<string>
  /** Why the pass declined to rule, where it did. */
  readonly refused?: string
}

export const nothing: Collected = { cases: [], flows: [], ledgers: [], scratch: [] }

/** A pass that declined to rule, and said why. */
const declined = (why: string): Collected => ({ ...nothing, refused: why })

/** Everything one plan collects, in the order a removal walks it. */
export const collected = (plan: Collected): ReadonlyArray<string> => [
  ...plan.cases,
  ...plan.flows,
  ...plan.ledgers,
  ...plan.scratch
]

export interface PlanRequest {
  readonly sessions: string
  readonly graceHours: number
  /**
   * Everything the store still stands behind, read from the LOWEST live
   * ceiling, or `null` where it stands behind nothing. A command pinned to an
   * old cut holds its rows open, so that reading keeps everything its cut can
   * name for as long as it runs.
   */
  readonly live: Cuts.Live | null
  /**
   * Whether any run is open at all. It is an INPUT because the reaping pass in
   * front of this one changes the answer, and it is what stands between a live
   * command and the side rows it was handed.
   */
  readonly anyOpen: boolean
}

/** What the cache may lose, read-only: nothing here removes anything. */
export const plan = Effect.fn("ContentGc.plan")(function*(request: PlanRequest) {
  const live = request.live
  if (live === null) return declined(NO_CUT)
  const fs = yield* FileSystem.FileSystem
  const cutoff = Date.now() - request.graceHours * 3_600_000
  const olderThan = (entry: string) =>
    fs.stat(entry).pipe(
      Effect.map((info) => Option.match(info.mtime, { onNone: () => false, onSome: (at) => at.getTime() < cutoff })),
      Effect.orElseSucceed(() => false)
    )
  const entries = (dir: string) => fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as Array<string>))

  const root = path.join(request.sessions, CONTENT)
  const cases: Array<string> = []
  const scratch: Array<string> = []
  for (const entry of yield* entries(root)) {
    const dir = path.join(root, entry)
    if (entry.startsWith(".writing-")) {
      if (yield* olderThan(dir)) scratch.push(dir)
      continue
    }
    if (!HASH.test(entry) || live.cases.has(entry)) continue
    if (yield* olderThan(dir)) cases.push(dir)
  }

  const flows: Array<string> = []
  const flowsRoot = path.join(root, FLOWS)
  for (const entry of yield* entries(flowsRoot)) {
    const file = path.join(flowsRoot, entry)
    if (entry.startsWith(".writing-")) {
      if (yield* olderThan(file)) scratch.push(file)
      continue
    }
    const hash = entry.endsWith(".json") ? entry.slice(0, -".json".length) : entry
    if (!HASH.test(hash) || live.flows.has(hash)) continue
    if (yield* olderThan(file)) flows.push(file)
  }

  // A merged side-row directory is named by no row, so the only thing that can
  // be reading one is a run that has not finished — and while any run is
  // open, none go.
  const ledgers: Array<string> = []
  const ledgerRoot = path.join(root, LEDGERS)
  for (const entry of yield* entries(ledgerRoot)) {
    const dir = path.join(ledgerRoot, entry)
    if (entry.startsWith(".writing-")) {
      if (yield* olderThan(dir)) scratch.push(dir)
      continue
    }
    if (!request.anyOpen && (yield* olderThan(dir))) ledgers.push(dir)
  }

  return { cases, flows, ledgers, scratch } satisfies Collected as Collected
})

/** What one cache sweep is worth saying. */
export const line = (plan: Collected, dryRun: boolean): string =>
  plan.refused !== undefined
    ? `[cleanup] _content untouched: ${plan.refused}`
    : `[cleanup] _content: ${dryRun ? "would drop" : "dropped"} ${plan.cases.length} case(s), ` +
      `${plan.flows.length} flows document(s), ${plan.ledgers.length} merged ledger(s) and ` +
      `${plan.scratch.length} half-written entr(ies) — every one of them rebuilds from a row`
