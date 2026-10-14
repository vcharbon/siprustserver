/**
 * ONE call's evidence, read the way the viewer's text API reads it: a cell's
 * recordings and pivot off its bundle, the capture's stored flows envelope off
 * the store, and the known-bug rules the cell's confrontation cited. Drawing
 * them as ladders is the caller's (`compare-report.ts` `Ladders`).
 *
 * A bundle reaped by retention, or a document the store no longer holds,
 * reads as absent — never as empty.
 */
import { Cuts, type Model } from "@sip/store"
import { E2e } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** The directory a pivot-replay cell of `caseId` on `lane` writes under its run's bundle. */
export const cellDirOf = (bundlePath: string, caseId: string, lane: string): string =>
  path.join(bundlePath, E2e.cellDirName({ case: caseId, shape: "pivot-replay", infra: lane }))

/** What a cell directory recorded: each leg's recording text, by leg, and its pivot. */
export interface CellRecording {
  readonly recording: Readonly<Record<string, string>>
  readonly pivot: unknown
}

/** A cell's recordings and pivot; legs it recorded none of are absent, an unreadable pivot is null. */
export const readCellRecording = Effect.fn("CallEvidence.readCellRecording")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const recordingDir = path.join(dir, "recording")
  const entries = yield* fs.readDirectory(recordingDir).pipe(Effect.orElseSucceed(() => [] as Array<string>))
  const recording: Record<string, string> = {}
  for (const entry of entries.filter((name) => name.endsWith(".jsonl")).sort()) {
    recording[entry.slice(0, -".jsonl".length)] = yield* fs.readFileString(path.join(recordingDir, entry)).pipe(
      Effect.orElseSucceed(() => "")
    )
  }
  const text = yield* fs.readFileString(path.join(dir, "pivot.json")).pipe(Effect.orElseSucceed(() => ""))
  const pivot = text === "" ? null : yield* Effect.try(() => JSON.parse(text) as unknown).pipe(
    Effect.orElseSucceed(() => null)
  )
  return { recording, pivot } satisfies CellRecording
})

/** Whether the cell directory is still on disk: retention reaps most bundles. */
export const cellOnDisk = (dir: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.exists(dir)).pipe(Effect.orElseSucceed(() => false))

/** A stored leg's own text: its row carries the separator on to the next leg. */
const legText = (text: string): string => text.replace(/[\s,]+$/, "")

const decoder = new TextDecoder()

/** A stored envelope as the document it is with its legs left out; throws where it does not parse. */
export const envelopeDocument = (envelope: Model.FlowsEnvelope): unknown =>
  JSON.parse(decoder.decode(envelope.head) + decoder.decode(envelope.tail)) as unknown

/** Stored leg rows as the leg objects they are; throws where one does not parse. */
export const legDocuments = (rows: ReadonlyArray<Uint8Array>): ReadonlyArray<unknown> =>
  rows.map((row) => JSON.parse(legText(decoder.decode(row))) as unknown)

/** A stored flows document's envelope, parsed once: what every call of the capture is read inside. */
export interface StoredEnvelope {
  readonly flowsHash: string
  readonly document: unknown
  /** How many legs stand between the envelope's halves; 0 for a document held whole. */
  readonly legs: number
}

/** The stored envelope of a flows document; null where the store holds none or it does not parse. */
export const storedEnvelope = Effect.fn("CallEvidence.storedEnvelope")(function*(flowsHash: string) {
  const cuts = yield* Cuts.Service
  const envelope = yield* cuts.flowsEnvelope(flowsHash)
  if (envelope === null) return null
  const document = yield* Effect.try(() => envelopeDocument(envelope)).pipe(Effect.orElseSucceed(() => null))
  return document === null ? null : { flowsHash, document, legs: envelope.legs } satisfies StoredEnvelope
})

/** A known-bug bless the cell's confrontation cited: the rule and the ticket it stands for. */
export interface KnownBug {
  readonly rule: string
  readonly ticket: string | null
  readonly records: number
}

/** The known-bug rules a cell's `confrontation.ndjson` cites, by rule; empty where the file is gone. */
export const knownBugs = Effect.fn("CallEvidence.knownBugs")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(path.join(dir, "confrontation.ndjson")).pipe(Effect.orElseSucceed(() => ""))
  const byRule = new Map<string, KnownBug>()
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue
    const row = yield* Effect.try(() => JSON.parse(line) as Record<string, unknown>).pipe(
      Effect.orElseSucceed(() => null)
    )
    if (row === null || row["class"] !== "known-bug" || typeof row["rule"] !== "string") continue
    const rule = row["rule"]
    const held = byRule.get(rule)
    byRule.set(rule, {
      rule,
      ticket: typeof row["ticket"] === "string" ? row["ticket"] : held?.ticket ?? null,
      records: (held?.records ?? 0) + 1
    })
  }
  return [...byRule.values()].sort((a, b) => a.rule.localeCompare(b.rule))
})
