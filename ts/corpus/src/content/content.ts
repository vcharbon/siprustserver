/**
 * The MATERIALISER: a case hash to a case directory on disk.
 *
 * `_content/<hash>/` is a disposable cache. Every byte in it rebuilds from the
 * `cases`, `case_documents`, `case_assets` and `ledger_rows` rows, so removing
 * `_content` costs a re-materialisation and nothing else. The hash names
 * everything the case carries, its side rows included, so an entry can never
 * serve content the store has since moved: a moved row is a different hash and
 * a different directory. What it lays out is exactly a hand-authored case
 * directory — `<id>.v3.json`, the `scenario.json` symlink to it, `resources/`
 * and the side rows where the deployment's layout puts them (`CaseLayout`) —
 * because `Case::load` must not be able to tell a materialised case from a
 * hand-authored one.
 *
 * The hash directory holds ONE child, and that child is the case: a cell's id is
 * the BASENAME of the path it cites, so a case cited straight at `<hash>/` would
 * be named by its hash in every cell directory, run-spec and case row. The name
 * is the id the document gives itself, which is content like the rest.
 *
 * Lazy and lock-free. A case is materialised when something asks for that hash,
 * one hash at a time, never by walking the corpus. Concurrency is settled by
 * NAMING rather than by exclusion: content is written into a temp directory and
 * renamed into place, so a directory that exists is complete by construction
 * and two materialisations of one hash cannot see each other's half-written
 * work.
 */
import { Cuts, type Model } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type { PlatformError } from "effect/PlatformError"
import * as Schema from "effect/Schema"
import * as crypto from "node:crypto"
import * as path from "node:path"
import { documentFile, linkDocument } from "../cut/case-dir.js"
import { CaseLayout } from "../deployment/index.js"

/** The cache, beside the sessions it serves. */
export const CONTENT = "_content"

/** Merged side-row directories, one per set of cases a batch runs together. */
export const LEDGERS = "_ledgers"

/** Flows documents, one file per hash: what a cell's confrontation reads. */
export const FLOWS = "_flows"

/** How many legs one query brings back, so a document is written a few MB at a time. */
const FLOWS_PAGE = 64

/** The store holds nothing under this content hash — a case's or a flows document's. */
export class ContentMissing extends Schema.TaggedError<ContentMissing>()("Corpus.ContentMissing", {
  hash: Schema.String
}) {}

const root = (sessions: string): string => path.join(sessions, CONTENT)

/** Where a case hash materialises. Answered whether or not it is there yet. */
export const caseDir = (sessions: string, caseHash: string): string => path.join(root(sessions), caseHash)

/**
 * The case directory inside a materialised hash — the path a cell cites, whose
 * basename is the case's own id. The hash directory holds exactly one child.
 */
export const caseIn = Effect.fn("Content.caseIn")(function*(contentDir: string) {
  const fs = yield* FileSystem.FileSystem
  const named = (yield* fs.readDirectory(contentDir))[0]
  if (named === undefined) return yield* Effect.die(new Error(`${contentDir} holds no case directory`))
  return path.join(contentDir, named)
})

/** Where a flows hash materialises. Answered whether or not it is there yet. */
export const flowsFile = (sessions: string, flowsHash: string): string =>
  path.join(root(sessions), FLOWS, `${flowsHash}.json`)

/** Where a SET of cases' merged side rows materialise: named by the set, so it dedupes too. */
export const ledgerDir = (sessions: string, caseHashes: ReadonlyArray<string>): string =>
  path.join(root(sessions), LEDGERS, crypto.createHash("sha256").update([...caseHashes].sort().join("\n")).digest("hex"))

/** A JSON document as a case directory holds it: pretty, newline-terminated. */
const writeJson = (fs: FileSystem.FileSystem, file: string, document: unknown) =>
  Effect.andThen(
    fs.makeDirectory(path.dirname(file), { recursive: true }),
    fs.writeFileString(file, `${JSON.stringify(document, null, 2)}\n`)
  )

/**
 * The temp directory a materialisation builds in: a sibling of the target, so
 * the rename that publishes it is a rename and not a copy.
 */
const scratch = (fs: FileSystem.FileSystem, under: string) =>
  Effect.andThen(
    fs.makeDirectory(under, { recursive: true }),
    fs.makeTempDirectory({ directory: under, prefix: ".writing-" })
  )

/**
 * `temp` published as `target`, or thrown away because somebody else published
 * the same content first. Either way the caller gets a complete `target`.
 */
const publish = (fs: FileSystem.FileSystem, temp: string, target: string): Effect.Effect<string, PlatformError> =>
  fs.rename(temp, target).pipe(
    Effect.catch((cause) =>
      Effect.andThen(
        fs.remove(temp, { recursive: true, force: true }),
        Effect.flatMap(fs.exists(target), (there) => (there ? Effect.void : Effect.fail(cause)))
      )
    ),
    Effect.as(target)
  )

/** The case id the document names itself by; the file and the symlink take it. */
const idOf = (document: unknown, caseHash: string): string => {
  const named = (document as { case?: { id?: unknown } } | null)?.case?.id
  return typeof named === "string" && named !== "" ? named : caseHash
}

const build = Effect.fn("Content.build")(function*(temp: string, content: Model.CaseContent, caseHash: string) {
  const fs = yield* FileSystem.FileSystem
  const layout = yield* CaseLayout.Service
  const id = idOf(content.document, caseHash)
  const dir = path.join(temp, id)
  yield* fs.makeDirectory(dir, { recursive: true })
  yield* writeJson(fs, path.join(dir, documentFile(id)), content.document)
  yield* linkDocument(dir, id)
  for (const asset of content.assets) {
    const file = path.join(dir, asset.path)
    yield* fs.makeDirectory(path.dirname(file), { recursive: true })
    yield* fs.writeFile(file, asset.bytes)
  }
  for (const row of content.ledger) {
    yield* writeJson(fs, path.join(dir, layout.rowsDir, layout.rowFile(row.number)), row.document)
  }
})

/**
 * One case on disk, and the directory a cell cites. Idempotent: a hash already
 * materialised is answered without reading a row, because the directory only
 * exists once it is whole.
 */
export const materialise = Effect.fn("Content.materialise")(function*(sessions: string, caseHash: string) {
  const fs = yield* FileSystem.FileSystem
  const target = caseDir(sessions, caseHash)
  if (yield* fs.exists(target)) return yield* caseIn(target)
  const cuts = yield* Cuts.Service
  const content = yield* cuts.content(caseHash)
  if (content === null) return yield* new ContentMissing({ hash: caseHash })
  const temp = yield* scratch(fs, root(sessions))
  yield* build(temp, content, caseHash)
  return yield* caseIn(yield* publish(fs, temp, target))
})

/**
 * One capture's flows document on disk, and its path — what a cell's post-run
 * confrontation reads its header references from. Written a leg at a time from
 * the rows it is kept in, and byte-identical to the document the cut read.
 */
export const materialiseFlows = Effect.fn("Content.materialiseFlows")(function*(sessions: string, flowsHash: string) {
  const fs = yield* FileSystem.FileSystem
  const target = flowsFile(sessions, flowsHash)
  if (yield* fs.exists(target)) return target
  const cuts = yield* Cuts.Service
  const envelope = yield* cuts.flowsEnvelope(flowsHash)
  if (envelope === null) return yield* new ContentMissing({ hash: flowsHash })
  return yield* materialiseFlowsFrom(sessions, flowsHash, envelope)
})

/** `bytes` appended to `out`; an empty part writes nothing (the platform refuses a zero-length write). */
const write = (out: FileSystem.File, bytes: Uint8Array) =>
  bytes.length === 0 ? Effect.void : out.writeAll(bytes)

/**
 * The same document, written from an ENVELOPE its reader already holds: a head
 * that carries the whole document, no leg row behind it, comes out of the store
 * once — not once to decide what shape the row is and once more to write it.
 */
export const materialiseFlowsFrom = Effect.fn("Content.materialiseFlowsFrom")(function*(
  sessions: string,
  flowsHash: string,
  envelope: Model.FlowsEnvelope
) {
  const fs = yield* FileSystem.FileSystem
  const target = flowsFile(sessions, flowsHash)
  if (yield* fs.exists(target)) return target
  const cuts = yield* Cuts.Service
  // The descriptor and the scratch are released whatever the paging does: a
  // query that fails does so with the file open and half the legs written.
  return yield* Effect.acquireUseRelease(
    scratch(fs, path.join(root(sessions), FLOWS)),
    (temp) =>
      Effect.gen(function*() {
        const file = path.join(temp, "flows.json")
        yield* Effect.scoped(Effect.gen(function*() {
          const out = yield* fs.open(file, { flag: "w" })
          yield* write(out, envelope.head)
          for (let from = 0; from < envelope.legs; from += FLOWS_PAGE) {
            for (const leg of yield* cuts.flowsLegs(flowsHash, from, FLOWS_PAGE)) yield* write(out, leg)
          }
          yield* write(out, envelope.tail)
        }))
        return yield* publish(fs, file, target)
      }),
    (temp) => fs.remove(temp, { recursive: true, force: true }).pipe(Effect.ignore)
  )
})

/**
 * ONE side-row directory holding every row these cases dial, and its path —
 * what a batch hands the interpreter (`CaseLayout.interpreterVariable`). A
 * row's number belongs to one case, so merging the rows of many cases cannot
 * collide.
 */
export const materialiseLedger = Effect.fn("Content.materialiseLedger")(function*(
  sessions: string,
  caseHashes: ReadonlyArray<string>
) {
  const fs = yield* FileSystem.FileSystem
  const layout = yield* CaseLayout.Service
  const target = ledgerDir(sessions, caseHashes)
  if (yield* fs.exists(target)) return target
  const cuts = yield* Cuts.Service
  const rows = yield* cuts.ledger(caseHashes)
  const temp = yield* scratch(fs, path.join(root(sessions), LEDGERS))
  for (const row of rows) yield* writeJson(fs, path.join(temp, layout.rowFile(row.number)), row.document)
  return yield* publish(fs, temp, target)
})
