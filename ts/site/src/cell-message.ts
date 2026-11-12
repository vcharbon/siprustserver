/**
 * One message of a replay, whole: by its ladder number (`m58`) or by the
 * flow step it was sent or received at (`s59`). For each replayed message its
 * ladder line, its leg and direction, then its headers and body; then the
 * step as the case document states it, and the captured message that step
 * cites in the capture's flows, where the capture has one. The read a reader
 * takes to see one SDP or one header, where the cell's whole evidence or the
 * unfolded run text is far larger than what it needs.
 */
import { pivotOf } from "@sip/corpus/report/call-ladders"
import { everyStep, isMessageEvent, type MessageEvent } from "@sip/viewer-core/layout"
import { buildRecordingLayout, type MessageRef, parseRecording, type RecordedMessage } from "@sip/viewer-core/recording"
import { ladderLines } from "@sip/viewer-core/text"
import { Effect } from "effect"
import { NotFound } from "./catalog.ts"
import type { TriageCatalog } from "./triage.ts"

/** Which message: a ladder number, or a step id. */
export interface Asked {
  readonly m?: number | undefined
  readonly step?: string | undefined
}

/** A capture leg window, as the flows read answers it. */
type LegsRead = (from: number) => Effect.Effect<TriageCatalog.StudyFlowsLegs, NotFound>

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined

const stepOf = (event: MessageEvent): string | undefined => {
  for (const decoration of event.decorations) if (decoration.kind === "step") return decoration.id
  return undefined
}

const wire = (message: RecordedMessage | undefined): string => {
  if (message === undefined || message.raw === undefined) return "no wire recorded for this message"
  if (!message.binary) return message.raw.replace(/\r\n/g, "\n").replace(/\s+$/, "")
  return `${message.raw.slice(0, message.headLength).replace(/\r\n/g, "\n").trimEnd()}\n\n(binary body, ${
    message.bytes - message.headLength
  } B, not shown)`
}

/** The replay layout of a cell, with each ladder line beside its recorded message. */
export const replayOf = (detail: TriageCatalog.StudyCellDetail) => {
  const recording = parseRecording(detail.recording)
  const layout = buildRecordingLayout(recording, { document: pivotOf(detail.pivot), title: detail.name })
  const lines = ladderLines(layout).lines.flatMap((line) =>
    isMessageEvent(line.event) ? [{ event: line.event, line: line.line.trim() }] : []
  )
  const bySource = new Map(recording.messages.map((message) => [message.source, message]))
  return {
    recording,
    lines: lines.map((one) => ({ ...one, step: stepOf(one.event), message: bySource.get(one.event.source) }))
  }
}

/**
 * The ladder entry of the message an audit finding names as the recording
 * holds it (`offending_message`: leg and seq); none where it names none.
 */
export const messageAt = (replay: ReturnType<typeof replayOf>, ref: MessageRef) =>
  replay.lines.find((one) => one.message?.leg === ref.leg && one.message.seq === ref.seq)

/** The message(s) asked of `detail` as text; nothing that matches is a 404. */
export const messageText = Effect.fn("CellMessage.text")(function*(
  detail: TriageCatalog.StudyCellDetail,
  asked: Asked,
  legs: LegsRead | undefined
) {
  const replay = replayOf(detail)
  const picked = asked.m !== undefined
    ? replay.lines.filter((one) => one.event.row === asked.m)
    : replay.lines.filter((one) => one.step !== undefined && one.step === asked.step)
  const steps = [
    ...new Set([
      ...picked.flatMap((one) => one.step === undefined ? [] : [one.step]),
      ...(asked.step === undefined ? [] : [asked.step])
    ])
  ]
  const document = pivotOf(detail.pivot)
  const documentSteps = document === undefined ? [] : everyStep(document.flow).filter((step) => steps.includes(step.id))
  if (picked.length === 0 && documentSteps.length === 0) {
    return yield* new NotFound({
      id: `${detail.name} ${asked.m === undefined ? asked.step ?? "" : `m${asked.m}`}`,
      note: `the ladder has m0–m${Math.max(0, replay.lines.length - 1)}`
    })
  }
  const sections: string[] = picked.map((one) =>
    [
      `== ${one.line.split(" ")[0]}${one.step === undefined ? "" : ` · step ${one.step}`}`,
      one.line,
      one.message === undefined
        ? "no recorded message"
        : `leg ${one.message.leg} · ${
          one.message.dir === "out" ? "sent by the leg's actor" : "received by the leg's actor"
        } · seq ${one.message.seq} · ${one.message.bytes} B`,
      "",
      wire(one.message)
    ].join("\n")
  )
  for (const step of documentSteps) {
    sections.push(`== case step ${step.id}, as the document states it\n${JSON.stringify(step, null, 2)}`)
    const observed = record(record(step)?.["observed"])
    const leg = observed?.["leg"]
    const msg = observed?.["msg"]
    if (legs === undefined || typeof leg !== "number" || typeof msg !== "number") continue
    const read = yield* legs(leg).pipe(Effect.option)
    const msgs = read._tag === "None" ? undefined : record(read.value.legs[0])?.["msgs"]
    const raw = Array.isArray(msgs) ? record(msgs[msg])?.["raw"] : undefined
    sections.push(
      `== captured message the step cites: capture leg ${leg} msg ${msg}\n` +
        (typeof raw === "string" ? raw.replace(/\r\n/g, "\n").replace(/\s+$/, "") : "not in the capture's flows")
    )
  }
  return `${sections.join("\n\n")}\n`
})
