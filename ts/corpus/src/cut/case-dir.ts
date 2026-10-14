/**
 * What a case DIRECTORY says about itself, and the one file a replay opens it by.
 *
 * `Case::load` reads a directory's document as `scenario.json`, so every case
 * directory a cell cites carries that link beside the document the cut named —
 * whether it was materialised from the store (`content.ts`), written by the
 * generator, or quarantined for the refusal falsifier. Three writers, one
 * convention, stated once.
 *
 * The readings of a document are pure over the parsed document; `readDocument`
 * is the one read.
 */
import { Flow } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"

/** The name a replay opens a case directory by. */
export const DOCUMENT_LINK = "scenario.json"

/** The document the generator writes, named by the case's own id. */
export const documentFile = (caseId: string): string => `${caseId}.v3.json`

/** The link a cell opens the directory by, pointing at the document beside it. */
export const linkDocument = Effect.fn("CaseDir.linkDocument")(function*(caseDir: string, caseId: string) {
  const fs = yield* FileSystem.FileSystem
  const link = path.join(caseDir, DOCUMENT_LINK)
  yield* fs.remove(link, { force: true })
  yield* fs.symlink(documentFile(caseId), link)
})

/** A case document, parsed: a JSON object, never its file name. */
export type Document = object

/** A case file that does not read as a JSON object. */
export class DocumentUnreadable extends Schema.TaggedError<DocumentUnreadable>()("Corpus.DocumentUnreadable", {
  file: Schema.String,
  detail: Schema.String
}) {
  override get message(): string {
    return `${this.file}: ${this.detail}`
  }
}

/** The case document at `caseFile`, parsed. */
export const readDocument = Effect.fn("CaseDir.readDocument")(function*(caseFile: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(caseFile).pipe(
    Effect.mapError((error) => new DocumentUnreadable({ file: caseFile, detail: error.message }))
  )
  const parsed = yield* Effect.try({
    try: () => JSON.parse(text) as unknown,
    catch: (cause) => new DocumentUnreadable({ file: caseFile, detail: String(cause) })
  })
  if (typeof parsed !== "object" || parsed === null) {
    return yield* new DocumentUnreadable({ file: caseFile, detail: "not a JSON object" })
  }
  return parsed satisfies Document
})

interface Shape {
  readonly actors?: ReadonlyArray<{ readonly kind?: string; readonly endpoint?: string }>
  readonly endpoints?: ReadonlyArray<{ readonly id?: string }>
  readonly flow?: ReadonlyArray<Flow.FlowNode>
}

const shapeOf = (document: Document | undefined): Shape => document ?? {}

/**
 * The endpoint a case's egress leaves by: the CALLED party's, where the document
 * names one other than the default. `undefined` means the lane's own default
 * stands.
 *
 * The called party is the `uas` actor, and where the platform reached no callee
 * at all it is the media resource — an announcement-only call has a `uac` and an
 * `mrf` and nothing else, and reading no egress off it sends the lane to a
 * default endpoint the document never declares.
 */
export const egressOf = (document: Document | undefined): string | undefined => {
  const actors = shapeOf(document).actors ?? []
  const called = actors.find((a) => a.kind === "uas") ?? actors.find((a) => a.kind === "mrf")
  return called?.endpoint === undefined || called.endpoint === "ep1" ? undefined : called.endpoint
}

/**
 * Whether the document ASSERTS nothing: no `expect` step anywhere in its flow.
 *
 * Such a document scripts messages out and gates on nothing coming back, so a
 * run of it passes by doing nothing. The refusal falsifier reads that as an
 * answer about the DOCUMENT and never as a refusal falsified — a rule cannot be
 * contradicted by a replay that made no claim.
 */
export const assertsNothing = (document: Document | undefined): boolean =>
  !(shapeOf(document).flow ?? [])
    .flatMap((node) => Flow.flowNodeSteps(node))
    .some((step) => step.op === "expect")

/**
 * Every endpoint the document names. A lane egresses by one of these or it
 * cannot run the case at all — which is an answer about the DOCUMENT and not a
 * crash, and the falsifier reads it as one.
 */
export const endpointsOf = (document: Document | undefined): ReadonlySet<string> =>
  new Set((shapeOf(document).endpoints ?? []).flatMap((e) => (e.id === undefined ? [] : [e.id])))
