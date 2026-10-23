/**
 * BACKFILLS: what the store reads and the rows recorded before it lack,
 * measured off what is still on disk.
 *
 * - Rule hits: the per-rule counts of every run whose bundle survives and was
 *   never measured — the history the record cannot have, since it starts the
 *   day it lands and retention keeps few bundles.
 * - Calls: the main errors of every run the queue reads whose failing rows
 *   carry none, re-measured off its bundle (the verdict is never rewritten),
 *   and the refusal rows of every standing cut, read off the cut's
 *   `excluded.json`.
 *
 * A run whose bundle is gone stays unmeasured: marking it read would claim a
 * zero nobody took. A cut whose file holds no record keeps its rows — it may
 * simply have refused nothing — and is asked again next time.
 */
import { Bundle, Measure, Refusals, RuleHits } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import { excludedFile, readRefusals } from "../cut/excluded.js"
import { CellHost } from "../deployment/index.js"
import type { Paths } from "../paths.js"
import { countBundle } from "../replay/rule-hits.js"

/** The rule hits of up to `limit` unmeasured runs, counted off their bundles. */
export const ruleHits = Effect.fn("Backfill.ruleHits")(function*(limit: number) {
  const hits = yield* RuleHits.Service
  const fs = yield* FileSystem.FileSystem
  const pending = yield* hits.unmeasured(limit)
  let measured = 0
  let gone = 0
  let cells = 0
  for (const run of pending) {
    if (!(yield* fs.exists(run.bundlePath).pipe(Effect.orElseSucceed(() => false)))) {
      gone += 1
      continue
    }
    const counted = yield* countBundle(run.bundlePath, run.lane)
    if (counted.cells === 0) {
      gone += 1
      continue
    }
    yield* hits.record(run.id, counted.hits)
    measured += 1
    cells += counted.cells
  }
  return { pending: pending.length, measured, gone, cells }
})

/** The main errors of the runs of `lane` (every lane where absent), then the standing cuts' refusals. */
export const calls = Effect.fn("Backfill.calls")(function*(paths: Paths, lane: string | undefined) {
  const measure = yield* Measure.Service
  const refusals = yield* Refusals.Service
  const host = yield* CellHost.Service
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path

  const pending = yield* measure.unmeasured(lane)
  let measured = 0
  let gone = 0
  let rows = 0
  for (const run of pending) {
    if (!(yield* fs.exists(path.join(run.bundlePath, "campaign.json")).pipe(Effect.orElseSucceed(() => false)))) {
      gone += 1
      continue
    }
    const reading = yield* Bundle.read(run.bundlePath, { charges: host.charges })
    if (reading.cases.length === 0) {
      gone += 1
      continue
    }
    rows += yield* measure.remeasure(run.id, reading.cases)
    measured += 1
    if (measured % 200 === 0) yield* Console.log(`[store] measured ${measured}…`)
  }

  const standing = yield* refusals.standing
  let recorded = 0
  let firstTime = 0
  let refused = 0
  for (const cut of standing) {
    const records = yield* readRefusals(excludedFile(paths.sessions, cut.capture))
    if (records.length === 0) continue
    refused += yield* refusals.record(cut.prepId, cut.capture, records)
    recorded += 1
    if (cut.rows === 0) firstTime += 1
  }
  return {
    errors: { pending: pending.length, measured, gone, rows },
    refusals: { standing: standing.length, recorded, firstTime, refused }
  }
})
