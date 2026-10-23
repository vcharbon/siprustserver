/**
 * The run-recording ladder: what a run actually put on the wire, laid out with
 * the same geometry as the scenario it came from, so the two compare visually.
 *
 * A recording has no alternatives and no groups — it is one thing that
 * happened — so every event sits in the root frame, ordered by arrival.
 */
import type { PivotV3 } from "../generated/pivot-v3.ts"
import { callBands } from "../layout/calls.ts"
import { dialogTags, type TagOwner } from "../layout/dialogs.ts"
import { rootFrame } from "../layout/frames.ts"
import { buildLifelines, sutLifelineId } from "../layout/lifelines.ts"
import {
  defaultLayoutOptions,
  ROOT_FRAME,
  type Decoration,
  type LadderEvent,
  type LadderLayout,
  type LayoutOptions,
  type Lifeline
} from "../layout/model.ts"
import { recordedDialogTags, recordedEventId } from "./dialogs.ts"
import type { RecordedMessage, RunRecording } from "./parse.ts"

export interface RecordingLayoutInput {
  /** The bundle's own `pivot.json`, when it has one: it names the actors. */
  readonly document?: PivotV3 | undefined
  readonly title?: string | undefined
  readonly subtitle?: string | undefined
}

/**
 * Without a document the recording only knows leg ids, so it draws one
 * lifeline per leg: the first leg on the caller side, the rest opposite.
 */
const syntheticLifelines = (legs: readonly string[], options: LayoutOptions): readonly Lifeline[] => {
  const doc = {
    actors: legs.map((leg) => ({ id: leg, kind: "uac" as const, endpoint: "ep0" })),
    legs: legs.map((leg, index) => ({ id: leg, actor: leg, dir: index === 0 ? ("out" as const) : ("in" as const) })),
    endpoints: [{ id: "ep0", observed: "", side: "peer" as const, binding: "dedicated" as const }]
  }
  return buildLifelines(doc, options)
}

const messageLabel = (message: RecordedMessage): string => {
  if (typeof message.status === "number") {
    return message.reason ? `${message.status} ${message.reason}` : String(message.status)
  }
  return message.method ?? "?"
}

const messageSublabel = (message: RecordedMessage): string | undefined => {
  const parts: string[] = []
  if (typeof message.status === "number" && message.cseqMethod) parts.push(message.cseqMethod)
  if (typeof message.cseq === "number") parts.push(`CSeq ${message.cseq}`)
  return parts.length === 0 ? undefined : parts.join(" · ")
}

export const buildRecordingLayout = (
  recording: RunRecording,
  input: RecordingLayoutInput = {},
  overrides: Partial<LayoutOptions> = {}
): LadderLayout => {
  const options = { ...defaultLayoutOptions, ...overrides }
  const warnings = [...recording.warnings]
  const doc = input.document
  const calls = doc ? callBands(doc) : undefined
  const dialogs = doc ? dialogTags(doc) : undefined
  // The wire's own tags name the dialogs; the scenario's early ids only stand
  // in for a line that carries no parsed message, or a leg that stated no tag.
  const originators = new Map<string, TagOwner>(
    (doc?.legs ?? []).map((leg) => [leg.id, leg.dir === "in" ? "sut" : "actor"])
  )
  const recorded = recordedDialogTags(recording, originators)
  const lifelines = doc
    ? buildLifelines(doc, options, calls?.byLeg)
    : syntheticLifelines(recording.legs, options)
  if (!doc) warnings.push("no pivot.json in the bundle: lifelines derived from leg ids alone")
  const sut = sutLifelineId(lifelines)
  const legActor = new Map((doc?.legs ?? []).map((leg) => [leg.id, leg.actor]))
  const frame = rootFrame(lifelines, options)

  // The scenario tokens the legend lists are the ones some event fell back on.
  const borrowed = new Set<string>()
  const events: LadderEvent[] = recording.messages.map((message, row) => {
    const actorId = legActor.get(message.leg) ?? message.leg
    const actorX = frame.lifelineX[actorId]
    if (actorX === undefined) warnings.push(`recorded leg "${message.leg}" has no lifeline`)
    const ax = actorX ?? frame.x0
    const sx = frame.lifelineX[sut] ?? frame.x1
    const fromActor = message.dir === "out"
    const decorations: Decoration[] = [{ kind: "observed", atUs: message.atUs }]
    if (message.stepId) decorations.push({ kind: "step", id: message.stepId })
    const call = calls?.byLeg.get(message.leg)
    const id = recordedEventId(message)
    const fallback = recorded.legs.has(message.leg) && message.raw !== undefined
      ? undefined
      : (message.stepId ? dialogs?.byStep.get(message.stepId) : undefined) ??
        dialogs?.byLeg.get(message.leg)
    const dialog = recorded.byMessage.get(id) ?? fallback
    if (dialog !== undefined && dialog === fallback) {
      borrowed.add(dialog.from.token)
      if (dialog.to !== undefined) borrowed.add(dialog.to.token)
    }
    return {
      kind: "message",
      id,
      row,
      y: options.headerHeight + row * options.rowHeight + options.rowHeight / 2,
      frameId: ROOT_FRAME,
      legId: message.leg,
      actorId,
      op: fromActor ? "send" : "expect",
      direction: fromActor ? "toSut" : "fromSut",
      fromLifeline: fromActor ? actorId : sut,
      toLifeline: fromActor ? sut : actorId,
      x1: fromActor ? ax : sx,
      x2: fromActor ? sx : ax,
      label: messageLabel(message),
      sublabel: messageSublabel(message),
      decorations,
      altId: undefined,
      branchName: undefined,
      groupId: undefined,
      dialog,
      callId: call?.id,
      callSlot: call?.slot,
      origin: "recording",
      source: message.source
    }
  })

  const lastLifeline = lifelines[lifelines.length - 1]
  return {
    title: input.title ?? "run recording",
    subtitle: input.subtitle ?? `${recording.legs.length} legs · ${recording.messages.length} messages`,
    lifelines,
    frames: { [ROOT_FRAME]: frame },
    events,
    blocks: [],
    calls: calls?.bands ?? [],
    dialogs: [...recorded.all, ...(dialogs?.all ?? []).filter((token) => borrowed.has(token.token))],
    rowCount: events.length,
    width: (lastLifeline?.x ?? options.marginX) + options.badgeGutter,
    height: options.headerHeight + events.length * options.rowHeight + options.footerHeight,
    options,
    warnings
  }
}
