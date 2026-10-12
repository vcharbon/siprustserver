/**
 * The flow STEP as the study's cross-reference: one id (`s7`) that the case
 * ladder, the replay ladder, the capture ladder and the confrontation table all
 * name the same message by.
 *
 * Each of the three ladders keys its own rows differently — the scenario by the
 * step id itself, the recording by `<leg>#<seq>`, the capture by its datagram
 * coordinate — so a click anywhere is turned into a step here, and a step is
 * turned back into the three selections. Nothing else knows the correspondence.
 */
import type { FlowsLayout, LadderEvent, LadderLayout } from "@sip/viewer-core"

/** The step a ladder event carries, wherever its layout put it. */
export const stepOfEvent = (event: LadderEvent): string | undefined => {
  if (event.origin === "scenario") return event.id
  for (const decoration of event.decorations) {
    if (decoration.kind === "step") return decoration.id
  }
  return undefined
}

/** Step id → the id of the ladder row drawing it. */
export type StepIndex = ReadonlyMap<string, string>

export const scenarioSteps = (layout: LadderLayout | undefined): StepIndex => {
  const index = new Map<string, string>()
  for (const event of layout?.events ?? []) index.set(event.id, event.id)
  return index
}

export const recordingSteps = (layout: LadderLayout | undefined): StepIndex => {
  const index = new Map<string, string>()
  for (const event of layout?.events ?? []) {
    const step = stepOfEvent(event)
    // First writing wins: a retransmitted step is drawn more than once, and the
    // one the reader is sent to is the one the interpreter reached the step on.
    if (step !== undefined && !index.has(step)) index.set(step, event.id)
  }
  return index
}

export const captureSteps = (layout: FlowsLayout | undefined): StepIndex => {
  const index = new Map<string, string>()
  for (const row of layout?.rows ?? []) {
    if (row.stepId !== undefined && !index.has(row.stepId)) index.set(row.stepId, row.id)
  }
  return index
}

/**
 * What the study has selected: one step, and the row each ladder draws it on.
 * A pane's own id survives a step that no other pane knows — clicking a
 * captured datagram no step cites still selects it, and simply focuses nothing
 * elsewhere.
 */
export interface Focus {
  readonly step?: string | undefined
  readonly capture?: string | undefined
  readonly replay?: string | undefined
  readonly scenario?: string | undefined
}

export interface StepIndexes {
  readonly capture: StepIndex
  readonly replay: StepIndex
  readonly scenario: StepIndex
}

/** The focus a step puts on every ladder that draws it. */
export const focusStep = (step: string, indexes: StepIndexes): Focus => ({
  step,
  capture: indexes.capture.get(step),
  replay: indexes.replay.get(step),
  scenario: indexes.scenario.get(step)
})
