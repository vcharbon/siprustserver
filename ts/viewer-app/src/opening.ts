/**
 * What a capture ladder is ABOUT: the legs it opens on and the SUT that orients
 * it, read from whatever names the call, in order of precedence — the cell's
 * bundle pivot, else the case's cut document (a cell the lane skipped has no
 * bundle), else the call's exclusion records (a refused call has no document),
 * else nothing, and the ladder opens on the capture's own first legs.
 */
import {
  captureSelection,
  documentSut,
  exclusionSelection,
  exclusionSut,
  parseExclusions,
  type CaptureSelection,
  type PivotV3
} from "@sip/viewer-core"
import type { TriageStudy } from "./api.ts"
import { readPivot, type DocumentRead } from "./document.ts"

export interface CaptureOpening {
  readonly selection: CaptureSelection | undefined
  readonly sut: readonly string[]
}

/** Nothing names a leg: the ladder opens on the capture's own first. */
export const NO_OPENING: CaptureOpening = { selection: undefined, sut: [] }

/**
 * The document a cell is drawn from: its bundle's pivot, else the standing
 * cut's document of its case. Read as a pivot either way, so a cell with no
 * bundle and a case with no document reads as no document, with the reason.
 */
export const cellDocument = (pivot: unknown, cutDocument: unknown): DocumentRead => readPivot(pivot ?? cutDocument)

/** A document's own opening: the legs its steps cite, the SUT its steps and endpoints state. */
export const openingOfDocument = (document: PivotV3): CaptureOpening => ({
  selection: captureSelection(document),
  sut: documentSut(document)
})

/**
 * The generator's records read as the selection a refused call's ladder draws:
 * the case's legs, the charged datagrams, and the SUT the cut stated, so the
 * ladder is that call's alone and oriented, as a cut case's is. With `caseId`
 * only that call's records count: the capture's file names every refused call.
 */
export const openingOfExclusions = (exclusions: TriageStudy["exclusions"], caseId?: string): CaptureOpening => {
  const read = exclusions === undefined ? undefined : parseExclusions(exclusions.document)
  if (read === undefined || !read.ok) return NO_OPENING
  const records = caseId === undefined ? read.records : read.records.filter((record) => record.caseId === caseId)
  const selection = exclusionSelection(records)
  return selection.empty ? NO_OPENING : { selection, sut: exclusionSut(records) }
}

/** A cell's opening: its document where it reads, else the call's own exclusion records. */
export const cellOpening = (
  document: DocumentRead,
  exclusions: TriageStudy["exclusions"],
  caseId: string
): CaptureOpening => document.ok ? openingOfDocument(document.document) : openingOfExclusions(exclusions, caseId)
