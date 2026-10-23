/**
 * One side of a view: a case or a run, loaded and laid out.
 *
 * The compare view needs both to look the same, and so does the selection
 * panel, so the difference between "a scenario" and "a recording" is reduced
 * here to one layout plus the payload the side panel shows.
 */
import {
  buildRecordingLayout,
  buildScenarioLayout,
  parseRecording,
  type LadderLayout
} from "@sip/viewer-core"
import { api, type CaseDetail, type RunDetail } from "./api.ts"
import { readPivot } from "./document.ts"
import type { SideRef } from "./router.ts"

export interface SideData {
  readonly ref: SideRef
  readonly origin: "scenario" | "recording"
  readonly title: string
  readonly layout: LadderLayout | undefined
  readonly problem: string | undefined
  readonly detail: CaseDetail | RunDetail
}

export const loadSide = async (ref: SideRef): Promise<SideData> => {
  if (ref.kind === "case") {
    const detail = await api.case(ref.id)
    const read = readPivot(detail.document)
    return {
      ref,
      origin: "scenario",
      title: detail.title,
      layout: read.ok ? buildScenarioLayout(read.document) : undefined,
      problem: read.ok ? undefined : read.reason,
      detail
    }
  }
  const detail = await api.run(ref.id)
  const read = readPivot(detail.pivot)
  const recording = parseRecording(detail.recording)
  return {
    ref,
    origin: "recording",
    title: `${detail.runId}`,
    layout: buildRecordingLayout(recording, {
      document: read.ok ? read.document : undefined,
      title: detail.runId,
      subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages`
    }),
    problem: undefined,
    detail
  }
}
