/**
 * The run BUNDLES under the session tree imported as run rows: every fact the
 * store holds about a run is already in that run's own output, so a lost
 * database costs a command, not evidence. The import is idempotent by bundle
 * path — re-running it imports only what arrived since.
 *
 * It goes capture by capture in run order, because recency IS id order: the
 * latest run of a capture is its highest id. That is why an import never
 * keeps the id a directory name claims — `run11` importing before some other
 * capture's `run3` would make the older run look like the newer one.
 * `bundle_path` stays authoritative for finding the evidence.
 */
import { Bundle, Resolver, Runs, StoreBinding } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Path from "effect/Path"
import { CellHost } from "../deployment/index.js"

interface BundleOnDisk {
  readonly capture: string
  readonly dir: string
}

/** What an import found and did. */
export interface Imported {
  readonly onDisk: number
  readonly imported: number
  readonly alreadyRecorded: number
}

/** The entries of `dir`, none where it does not read. */
const entriesOf = (fs: FileSystem.FileSystem, dir: string) =>
  fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))

/** The capture a bundle belongs to: its own case ids say so; the session dir is the fallback. */
const captureOf = Effect.fn("BundleImport.captureOf")(function*(
  sessionDir: string,
  stem: string,
  caseIds: ReadonlyArray<string>
) {
  const { caseId } = Resolver.caseShapes((yield* StoreBinding.Service).grammar)
  for (const id of caseIds) {
    const cut = caseId.exec(id)
    if (cut !== null) return cut[1]!
  }
  const fs = yield* FileSystem.FileSystem
  const listed = (yield* entriesOf(fs, sessionDir)).find((entry) =>
    entry.startsWith(stem) && entry !== stem && !/^run\d+$/.test(entry)
  )
  return listed ?? `${stem}.pcap.gz`
})

/** Every run bundle under the session tree, capture by capture in run order. */
const listBundles = Effect.fn("BundleImport.listBundles")(function*(sessions: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const out: Array<BundleOnDisk> = []
  for (const stem of [...(yield* entriesOf(fs, sessions))].sort()) {
    const sessionDir = path.join(sessions, stem)
    const info = yield* fs.stat(sessionDir).pipe(Effect.option)
    if (Option.isNone(info) || info.value.type !== "Directory") continue
    const runs = (yield* entriesOf(fs, sessionDir))
      .map((entry) => ({ entry, n: /^run(\d+)$/.exec(entry)?.[1] }))
      .filter((r): r is { entry: string; n: string } => r.n !== undefined)
      .sort((a, b) => Number(a.n) - Number(b.n))
    for (const { entry } of runs) out.push({ capture: stem, dir: path.join(sessionDir, entry) })
  }
  return out as ReadonlyArray<BundleOnDisk>
})

/** A bundle's own timestamps: its `campaign.json` states when it started, the tree when it ended. */
const timestamps = Effect.fn("BundleImport.timestamps")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const rollup = yield* fs.readFileString(path.join(dir, "campaign.json")).pipe(
    Effect.map((text) => JSON.parse(text) as { ts?: unknown }),
    Effect.orElseSucceed(() => ({}) as { ts?: unknown })
  )
  const info = yield* fs.stat(dir).pipe(Effect.option)
  const mtime = Option.isSome(info) ? Option.getOrUndefined(info.value.mtime) : undefined
  return {
    started: typeof rollup.ts === "string" ? rollup.ts : undefined,
    finished: mtime?.toISOString()
  }
})

/** Every bundle under `sessions` no row records yet, imported; a bundle stating no lane is `lane`'s. */
export const importBundles = Effect.fn("BundleImport.importBundles")(function*(sessions: string, lane: string) {
  const runs = yield* Runs.Service
  const host = yield* CellHost.Service
  const path = yield* Path.Path
  const known = new Set((yield* runs.allBundles).map((row) => row.bundlePath))
  const bundles = yield* listBundles(sessions)
  let imported = 0
  let alreadyRecorded = 0
  for (const bundle of bundles) {
    if (known.has(bundle.dir)) {
      alreadyRecorded += 1
      continue
    }
    const reading = yield* Bundle.read(bundle.dir, { charges: host.charges })
    const capture = yield* captureOf(
      path.dirname(bundle.dir),
      bundle.capture,
      reading.cases.map((entry) => entry.caseId)
    )
    const when = yield* timestamps(bundle.dir)
    yield* runs.importRun({
      capture,
      lane: reading.lane ?? lane,
      mode: reading.mode ?? "record",
      status: reading.status,
      bundlePath: bundle.dir,
      ...(when.started === undefined ? {} : { startedAt: when.started }),
      ...(when.finished === undefined ? {} : { finishedAt: when.finished }),
      cases: reading.cases
    })
    imported += 1
    if (imported % 100 === 0) yield* Console.log(`[store] imported ${imported}…`)
  }
  return { onDisk: bundles.length, imported, alreadyRecorded } satisfies Imported
})
