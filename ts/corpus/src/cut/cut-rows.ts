/**
 * What the cut wrote to its SCRATCH, read back as cut-store rows.
 *
 * One direction only: the generator owns the case directories and knows
 * nothing of the database, so this reads its output — the enriched flows
 * document, each case's document, its `resources/` and its side rows
 * (`CaseLayout`) — and hands it to `Cuts.record`. The document is PARSED,
 * never shipped as bytes, so it hashes through the same canonical-JSON path a
 * replay's document does and the two land on one `case_documents` row. The
 * side rows go WITH it: the case is named over them too, so a cut that moved
 * only a row is a case the store has never seen. The case's CALL KEY is read
 * off its side rows (`CaseLayout.callKeyOf`): the cut states each call's name
 * once, where it dials it.
 *
 * The rows ARE the cut: the scratch goes as soon as they are written. A
 * capture the store refuses is named on stderr and counted, and stands at
 * whatever cut it stood at before — one capture's rows never cost the
 * captures beside it theirs.
 */
import { Cuts, type Model } from "@sip/store"
import { FlowsSegments } from "@sip/pipeline"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { CaseLayout } from "../deployment/index.js"
import { documentFile } from "./case-dir.js"
import { type CutCase, type CutReading, NAMES_FILE, type NameLine } from "./cut-contract.js"
import { readRefusals } from "./excluded.js"

export { NAMES_FILE }

/** Where a case keeps the files it carries beside its document. */
const RESOURCES = "resources"

/** Every file under `dir`, in path order, named relative to `base`. */
const filesUnder = (
  fs: FileSystem.FileSystem,
  base: string,
  dir: string
): Effect.Effect<ReadonlyArray<string>, never> =>
  Effect.gen(function*() {
    const found: Array<string> = []
    for (const entry of yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))) {
      const child = path.join(dir, entry)
      if (yield* fs.readLink(child).pipe(Effect.as(true), Effect.orElseSucceed(() => false))) continue
      const info = yield* fs.stat(child).pipe(Effect.orElseSucceed(() => undefined))
      if (info?.type === "Directory") found.push(...(yield* filesUnder(fs, base, child)))
      else if (info?.type === "File") found.push(path.relative(base, child))
    }
    return found.sort()
  })

/**
 * The call key a case's side rows state. A case dials exactly one row, and a
 * case with none or several names no call the store can resolve: that case
 * fails, and with it the capture's reading — never a key made up here.
 */
const callKeyOf = (
  layout: CaseLayout.Interface,
  caseId: string,
  rows: ReadonlyArray<Model.LedgerRow>
): Effect.Effect<string> => {
  const keys = new Set(rows.flatMap((row) => {
    const key = layout.callKeyOf(row)
    return key === undefined ? [] : [key]
  }))
  if (keys.size !== 1) {
    return Effect.die(new Error(`case '${caseId}' states ${keys.size} call key(s) on its ledger rows, expected one`))
  }
  return Effect.succeed([...keys][0]!)
}

/** The cut's {@link NAMES_FILE} lines, by case id; none where it wrote no file. */
export const readNames = Effect.fn("CutRows.readNames")(function*(file: string | undefined) {
  if (file === undefined) return new Map<string, NameLine>() as ReadonlyMap<string, NameLine>
  const fs = yield* FileSystem.FileSystem
  if (!(yield* fs.exists(file))) return new Map<string, NameLine>() as ReadonlyMap<string, NameLine>
  const text = yield* fs.readFileString(file)
  return new Map(Object.entries(JSON.parse(text) as Record<string, NameLine>)) as ReadonlyMap<string, NameLine>
})

/** What the names file says of one case id, as request fields. */
const named = (line: NameLine | undefined) => ({
  ...(line?.start === undefined ? {} : { start: line.start }),
  ...(line?.replaces === undefined ? {} : { replaces: line.replaces })
})

const readJson = (fs: FileSystem.FileSystem, file: string) =>
  Effect.flatMap(fs.readFileString(file), (text) => Effect.sync(() => JSON.parse(text) as unknown))

/** One case directory, whole: the document and everything hung off its hash. */
const caseOf = (entry: CutCase, names: ReadonlyMap<string, NameLine>) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const layout = yield* CaseLayout.Service
    const caseDir = entry.dir
    const document = yield* readJson(fs, path.join(caseDir, documentFile(entry.id)))
    const assets: Array<Model.CutCaseRequest["assets"][number]> = []
    for (const file of yield* filesUnder(fs, caseDir, path.join(caseDir, RESOURCES))) {
      assets.push({ path: file, bytes: new Uint8Array(yield* fs.readFile(path.join(caseDir, file))) })
    }
    const ledger: Array<Model.LedgerRow> = []
    for (const file of yield* filesUnder(fs, caseDir, path.join(caseDir, layout.rowsDir))) {
      if (!file.endsWith(".json")) continue
      ledger.push({ number: path.basename(file, ".json"), document: yield* readJson(fs, path.join(caseDir, file)) })
    }
    return {
      caseId: entry.id,
      callKey: yield* callKeyOf(layout, entry.id, ledger),
      document,
      assets,
      ledger,
      egressEndpoint: entry.egressEndpoint ?? null,
      ...named(names.get(entry.id))
    } satisfies Model.CutCaseRequest
  })

/**
 * The enriched flows document as the store keeps it: the envelope's head, each
 * leg, then the tail. Read afresh on each pass, so a document of a gigabyte
 * costs one file descriptor rather than a buffer.
 */
const flowsOf = (file: string): Model.FlowsSource => () => FlowsSegments.segments(file)

/**
 * One capture's reading as a store request. `broke` and an empty case list are
 * carried apart exactly as the reading carries them: a cut that BROKE is a
 * failure, a cut that refused every case is an answer — and what it refused
 * goes with it, off the `excluded.json` the cut left.
 */
export const requestOf = Effect.fn("CutRows.requestOf")(function*(prepId: number, reading: CutReading) {
  const fs = yield* FileSystem.FileSystem
  const names = yield* readNames(reading.names)
  const cases = yield* Effect.forEach(reading.cases, (entry) => caseOf(entry, names))
  const refusals = yield* readRefusals(reading.excluded)
  return {
    prepId,
    capture: reading.capture,
    flows: (yield* fs.exists(reading.flows)) ? flowsOf(reading.flows) : null,
    broke: reading.broke ?? null,
    cutFingerprint: reading.cutFingerprint,
    cases,
    refusals: refusals.map((refusal) => {
      const replaces = names.get(refusal.caseId)?.replaces
      return replaces === undefined ? refusal : { ...refusal, replaces }
    })
  } satisfies Model.CaptureCutRequest
})

/** What mirroring a cut into rows did, for the phase's summary line. */
export interface Mirrored {
  readonly recorded: number
  /** Captures whose rows could not be written; each stands at its previous cut. */
  readonly failed: number
  readonly cases: number
  /** Cases the store had never seen; the rest are content it already holds, whole. */
  readonly fresh: number
}

export const NOTHING_MIRRORED: Mirrored = { recorded: 0, failed: 0, cases: 0, fresh: 0 }

export const plus = (a: Mirrored, b: Mirrored): Mirrored => ({
  recorded: a.recorded + b.recorded,
  failed: a.failed + b.failed,
  cases: a.cases + b.cases,
  fresh: a.fresh + b.fresh
})

const firstLine = (text: string): string => text.split("\n")[0] ?? text

/**
 * Every capture the cut read, mirrored into rows. Each capture stands alone:
 * whatever one throws is its own line on stderr and its own count, and the
 * capture behind it is still recorded.
 */
export const mirror = Effect.fn("CutRows.mirror")(function*(
  prepId: number,
  readings: ReadonlyMap<string, CutReading>
) {
  const cuts = yield* Cuts.Service
  let mirrored = NOTHING_MIRRORED
  for (const [capture, reading] of readings) {
    const outcome = yield* requestOf(prepId, reading).pipe(
      Effect.flatMap((request) => cuts.record(request)),
      Effect.catchCause((cause) => Effect.succeed(firstLine(Cause.pretty(cause))))
    )
    if (typeof outcome === "string") {
      yield* Console.error(`[cut] ${capture}: cut-store write failed, it stands at its previous cut — ${outcome}`)
      mirrored = plus(mirrored, { ...NOTHING_MIRRORED, failed: 1 })
      continue
    }
    mirrored = plus(mirrored, {
      recorded: 1,
      failed: 0,
      cases: outcome.cases.length,
      fresh: outcome.cases.filter((entry) => entry.fresh).length
    })
  }
  return mirrored
})
