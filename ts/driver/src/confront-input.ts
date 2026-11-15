/**
 * What the post-run confrontation reads of one finished replay cell, gathered
 * from the files the cell and its case leave on disk: the recordings, the
 * captured legs the document's `expect` steps cite, the bodies the steps
 * state, and the run's media mode. The driver confronts a cell through it, and
 * a reader re-confronting a stored cell reads the same inputs the same way.
 */
import { Body, Bundle, type Campaign, Flows, Pivot } from "@sip/contracts"
import { type Confront, FlowsFile } from "@sip/pipeline"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import { RUN_CONFIG_FILE } from "./layout.js"
import { readRecordings } from "./recordings.js"

/** The v3 document a cell names: a file, or `scenario.json` inside a case directory. */
export const documentPath = Effect.fn("Driver.documentPath")(function* (target: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const info = yield* fs.stat(target)
  return info.type === "Directory" ? path.join(target, "scenario.json") : target
})

/** The flows legs the document's `expect` steps cite through their `observed` coordinate. */
export const observedLegs = (pivot: Pivot.PivotV3): ReadonlySet<number> =>
  new Set(
    Pivot.pivotSteps(pivot).flatMap((step) =>
      step.op === "expect" && step.observed !== undefined ? [step.observed.leg] : []
    )
  )

/**
 * Every resource a `send` or `expect` step's body or part references, read
 * from the case directory as bytes and keyed by its ref: what the
 * confrontation holds the received bodies against (an expected one must
 * exist), and what the capture drove into the system (a sent one the
 * directory lacks is left out).
 */
export const readStatedBodies = Effect.fn("Driver.readStatedBodies")(function* (
  caseDir: string,
  pivot: Pivot.PivotV3
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const out = new Map<string, Uint8Array>()
  for (const step of Pivot.pivotSteps(pivot)) {
    const body = step.msg.body
    if ((step.op !== "expect" && step.op !== "send") || body === undefined) continue
    const refs = Body.isResourceBody(body)
      ? [body.ref]
      : Body.isMultipartBody(body)
        ? body.multipart.parts.map((part) => part.ref)
        : []
    for (const ref of refs) {
      if (out.has(ref)) continue
      const file = path.join(caseDir, ref)
      if (step.op === "send" && !(yield* fs.exists(file))) continue
      out.set(ref, yield* fs.readFile(file))
    }
  }
  return out
})

/**
 * The confrontation's input for the cell whose bundle is `absolute`: its
 * recordings and run configuration, the case's stated bodies, and the flows
 * legs the case cites where the cell names a flows document. A flows document
 * of a schema version this reader does not hold fails.
 */
export const confrontInputOf = Effect.fn("Driver.confrontInputOf")(function* (
  cell: Pick<Campaign.PivotReplayCell, "case" | "flows">,
  absolute: string,
  pivot: Pivot.PivotV3,
  verdict: Bundle.RunVerdict
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const recordings = yield* readRecordings(absolute)
  const captured = cell.flows === undefined
    ? undefined
    : yield* FlowsFile.readFlowsLegsDecoded(cell.flows, observedLegs(pivot))
  if (captured !== undefined) yield* Flows.requireSchemaVersion(captured.envelope)
  const resources = yield* readStatedBodies(path.dirname(yield* documentPath(cell.case)), pivot)
  const config = yield* Bundle.decodeRunConfig(
    JSON.parse(yield* fs.readFileString(path.join(absolute, RUN_CONFIG_FILE))) as unknown
  )
  return {
    pivot,
    verdict,
    recordings,
    resources,
    media: Bundle.mediaModeOf(config),
    ...(captured === undefined ? {} : { captured: captured.legs })
  } satisfies Confront.ConfrontInput
})
