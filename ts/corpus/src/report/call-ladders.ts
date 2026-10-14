/**
 * ONE call's ladders, drawn the way the viewer's text API draws them: the
 * replay ladder off a cell's recording, the reference ladder off the capture's
 * stored flows document. The drawing is `@sip/viewer-core`'s pure layouts and
 * text; the reading is `call-evidence.ts`. A document the store no
 * longer holds, or one that is no v3 pivot, draws no ladder — never an empty one.
 */
import {
  buildFlowsLayout,
  captureSelection,
  chunkedFlows,
  documentSut,
  type FlowsChunk,
  legWindows,
  parseFlowsDocument
} from "@sip/viewer-core/flows"
import type { PivotV3 } from "@sip/viewer-core/pivot"
import { buildRecordingLayout, parseRecording } from "@sip/viewer-core/recording"
import { flowsToText, ladderToText } from "@sip/viewer-core/text"
import { Cuts } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as CallEvidence from "./call-evidence.js"
import * as CompareReport from "./compare-report.js"

/** `value` as a v3 pivot; undefined where it is not one. */
export const pivotOf = (value: unknown): PivotV3 | undefined => {
  if (typeof value !== "object" || value === null) return undefined
  return (value as Record<string, unknown>)["pivot_version"] === 3 ? value as PivotV3 : undefined
}

/** The replay ladder of one cell, line by line, as the run text export renders it. */
export const replayLadder = (cell: CallEvidence.CellRecording, title: string): ReadonlyArray<string> => {
  const recording = parseRecording(cell.recording)
  const layout = buildRecordingLayout(recording, {
    document: pivotOf(cell.pivot),
    title,
    subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages`
  })
  return ladderToText(layout).replace(/\n+$/, "").split("\n")
}

/** A stored flows document read back over the legs `legs` names, in the windows the viewer reads it by. */
export const storedFlows = Effect.fn("CallLadders.storedFlows")(function*(
  envelope: CallEvidence.StoredEnvelope,
  legs: ReadonlyArray<number>
) {
  const cuts = yield* Cuts.Service
  const flowsHash = envelope.flowsHash
  if (envelope.legs === 0) return envelope.document
  const chunks = yield* Effect.forEach(legWindows(envelope.legs, legs), (window) =>
    cuts.flowsLegs(flowsHash, window.from, window.count).pipe(
      Effect.flatMap((rows) => Effect.try(() => CallEvidence.legDocuments(rows))),
      Effect.map((read): FlowsChunk => ({ from: window.from, legs: read })),
      Effect.orElseSucceed((): FlowsChunk => ({ from: window.from, legs: [] }))
    ))
  return chunkedFlows(envelope.document, envelope.legs, chunks)
})

/**
 * The reference ladder of one call: the capture's legs the case document
 * selects, oriented on the SUT it names, as the run text export draws a
 * one-call capture pane. Undefined where the document is no v3 pivot or the
 * flows document does not read.
 */
export const referenceLadder = (
  flows: unknown,
  document: unknown,
  title: string
): ReadonlyArray<string> | undefined => {
  const pivot = pivotOf(document)
  if (pivot === undefined) return undefined
  const read = parseFlowsDocument(flows)
  if (!read.ok) return undefined
  const layout = buildFlowsLayout(read.document, {
    selection: captureSelection(pivot),
    sut: documentSut(pivot),
    onlySelected: true,
    title
  })
  return flowsToText(layout).replace(/\n+$/, "").split("\n")
}

/** The legs of the capture a case document selects: what its reference ladder reads. */
export const selectedLegs = (document: unknown): ReadonlyArray<number> => {
  const pivot = pivotOf(document)
  return pivot === undefined ? [] : [...captureSelection(pivot).legs]
}

/** The compare report's ladders, drawn as the viewer draws them. */
export const layer = Layer.effect(
  CompareReport.Ladders,
  Effect.map(Cuts.Service, (cuts) =>
    CompareReport.Ladders.of({
      replay: replayLadder,
      reference: (envelope, document, title) =>
        storedFlows(envelope, selectedLegs(document)).pipe(
          Effect.map((flows) => referenceLadder(flows, document, title) ?? null),
          Effect.provideService(Cuts.Service, cuts)
        )
    }))
)
