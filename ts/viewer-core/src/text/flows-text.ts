/**
 * The capture ladder as text, in the line shape `source.txt` uses, so a ladder
 * read in the browser can be pasted where that file would have been.
 *
 * It renders the layout, not the document: the rows a reader narrowed to are
 * the rows the text carries, and the subtitle already states how it got there.
 */
import type { FlowsLayout, FlowsRow } from "../flows/layout.ts"
import { flowsMessageLabel, type FlowsMessage } from "../flows/parse.ts"
import { microsecondOffset } from "./offset.ts"

/** A request states the URI it was aimed at; a response states its status line. */
const messageColumn = (message: FlowsMessage): string =>
  message.status === undefined && message.uri !== undefined
    ? `${flowsMessageLabel(message)} ${message.uri}`
    : flowsMessageLabel(message)

const tagColumn = (message: FlowsMessage): string =>
  `${message.fromTag ?? "-"}/${message.toTag ?? "-"}`

const rowLine = (row: FlowsRow, bytes: boolean): string => {
  const message = row.message
  const columns = [
    messageColumn(message),
    message.cseqSeq === undefined
      ? "-"
      : `CSeq ${message.cseqSeq} ${message.cseqMethod ?? "-"}`,
    tagColumn(message),
    ...(bytes ? [`${message.bytes}B`] : [])
  ]
  const head =
    `m${row.row} ${microsecondOffset(row.atMs * 1000)} leg${row.legIndex}/${message.msgIndex} ` +
    `${message.src} -> ${message.dst}`
  return `  ${head} | ${columns.join(" | ")}${row.retx ? " RETX" : ""}`
}

/** Renders a capture ladder: title, subtitle, one line per drawn message. */
export const flowsToText = (layout: FlowsLayout): string => {
  const bytes = layout.rows.some((row) => row.message.bytes > 0)
  const header =
    `ladder (${layout.rows.length} msgs; m<idx> +t leg<leg>/<msg> src -> dst | message | CSeq | ` +
    `from-tag/to-tag${bytes ? " | bytes" : ""}):`
  return [
    layout.title,
    layout.subtitle,
    ...layout.warnings.map((warning) => `warning: ${warning}`),
    "",
    header,
    ...layout.rows.map((row) => rowLine(row, bytes)),
    ""
  ].join("\n")
}
