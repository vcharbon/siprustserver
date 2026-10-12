/**
 * A scenario or replay ladder as text, in the same line shape the capture
 * ladder uses, so the two can be read — and diffed — side by side.
 *
 * The columns are the ones the layout actually carries: a scenario states no
 * observed time, a bare recording states no dialog, and a missing column is
 * left out of every line rather than filled with an invented value.
 */
import {
  isMessageEvent,
  type InjectEvent,
  type LadderEvent,
  type LadderLayout,
  type MessageEvent
} from "../layout/model.ts"
import { microsecondOffset } from "./offset.ts"

/** The capture coordinate a ladder event carries, where it carries one. */
export const observedUs = (event: LadderEvent): number | undefined => {
  for (const decoration of event.decorations) {
    if (decoration.kind === "observed") return decoration.atUs
  }
  return undefined
}

const stepId = (event: LadderEvent): string | undefined => {
  for (const decoration of event.decorations) {
    if (decoration.kind === "step") return decoration.id
  }
  return undefined
}

interface Columns {
  readonly time: boolean
  readonly detail: boolean
  readonly dialog: boolean
  readonly step: boolean
}

const messageLine = (
  event: MessageEvent,
  label: (id: string) => string,
  time: string,
  columns: Columns
): string => {
  const parts = [
    event.label,
    ...(columns.detail ? [event.sublabel ?? "-"] : []),
    ...(columns.dialog ? [event.dialog?.token ?? "-"] : []),
    ...(columns.step ? [stepId(event) ?? event.id] : [])
  ]
  const head = `m${event.row}${time} ${label(event.fromLifeline)} -> ${label(event.toLifeline)}`
  return `  ${head} | ${parts.join(" | ")}`
}

const injectLine = (event: InjectEvent, time: string): string =>
  `  m${event.row}${time} inject ${event.action}${event.target === undefined ? "" : ` ${event.target}`}`

/** One rendered ladder line, still joined to the event it renders. */
export interface LadderLine {
  readonly event: LadderEvent
  readonly line: string
}

/** The header and the lines of a ladder layout, before they are joined. */
export interface LadderText {
  readonly head: readonly string[]
  readonly lines: readonly LadderLine[]
}

/**
 * A ladder layout rendered line by line, so a caller that puts something
 * BETWEEN two lines — a message body, an HTTP exchange — keeps the one line
 * shape every ladder text uses.
 */
export const ladderLines = (layout: LadderLayout): LadderText => {
  const labels = new Map(layout.lifelines.map((lifeline) => [lifeline.id, lifeline.label]))
  const label = (id: string): string => labels.get(id) ?? id
  const observed = layout.events.map(observedUs).filter((us): us is number => us !== undefined)
  const t0 = observed.length === 0 ? undefined : Math.min(...observed)
  const columns: Columns = {
    time: t0 !== undefined,
    detail: layout.events.some((event) => isMessageEvent(event) && event.sublabel !== undefined),
    dialog: layout.events.some((event) => isMessageEvent(event) && event.dialog !== undefined),
    step: layout.events.some(isMessageEvent)
  }
  const timeOf = (event: LadderEvent): string => {
    if (t0 === undefined) return ""
    const us = observedUs(event)
    return us === undefined ? " +-.------" : ` ${microsecondOffset(us - t0)}`
  }
  const header =
    `ladder (${layout.events.length} events; m<idx>${columns.time ? " +t" : ""} from -> to | ` +
    `message${columns.detail ? " | detail" : ""}${columns.dialog ? " | dialog" : ""}` +
    `${columns.step ? " | step" : ""}):`
  return {
    head: [
      layout.title,
      ...(layout.subtitle === undefined ? [] : [layout.subtitle]),
      ...layout.warnings.map((warning) => `warning: ${warning}`),
      "",
      header
    ],
    lines: layout.events.map((event) => ({
      event,
      line: isMessageEvent(event)
        ? messageLine(event, label, timeOf(event), columns)
        : injectLine(event, timeOf(event))
    }))
  }
}

/** Renders a ladder layout: title, subtitle, one line per event. */
export const ladderToText = (layout: LadderLayout): string => {
  const rendered = ladderLines(layout)
  return [...rendered.head, ...rendered.lines.map((row) => row.line), ""].join("\n")
}
