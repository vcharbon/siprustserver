/**
 * The capture ladder, with the datagram the reader picked shown above it.
 *
 * The bytes belong beside the arrows that carry them: a capture is read by
 * clicking a message and comparing what it says with the replay's own, so this
 * pane keeps its selection to itself rather than posting it to a shared panel
 * at the foot of the page.
 *
 * Its narrowings — the case's legs, one leg, a socket, a tag — are all the same
 * gesture, and none of them hides anything permanently: a capture is triage
 * evidence, so every control here is reversible in one click.
 */
import {
  bodySegments,
  CaptureLegend,
  FlowsLadder,
  flowsToText,
  type FlowsLayout,
  type FlowsRow
} from "@sip/viewer-core"
import type { JSX } from "react"
import { CopyText } from "./CopyText.tsx"
import { useScrollToRow } from "./scroll.ts"
import { WindowLine, type DrawnWindow } from "./WindowLine.tsx"
import { SipMessageView } from "./SipMessageView.tsx"

/** The row's coordinates and its wire bytes, in the shape the SIP view reads. */
const capturedRecord = (row: FlowsRow): Record<string, unknown> => ({
  leg: row.legIndex,
  msg: row.message.msgIndex,
  at: `${row.atMs.toFixed(1)}ms`,
  ...(row.stepId === undefined ? {} : { step: row.stepId }),
  src: row.message.src,
  dst: row.message.dst,
  ...(row.retx ? { retransmission: true } : {}),
  callId: row.message.callId,
  ...(row.dialog?.from === undefined ? {} : { fromTag: row.dialog.from.value }),
  ...(row.dialog?.to === undefined ? {} : { toTag: row.dialog.to.value }),
  bytes: row.message.bytes,
  ...(row.message.binary ? { body: `binary, ${row.message.body?.contentType ?? "unknown type"}` } : {}),
  raw: row.message.raw
})

export interface FlowsPaneProps {
  readonly layout: FlowsLayout
  readonly selected: FlowsRow | undefined
  readonly onSelect: (row: FlowsRow) => void
  /** The "only selected legs" control, where the caller owns one. */
  readonly onlySelected?: {
    readonly value: boolean
    readonly onChange: (next: boolean) => void
  } | undefined
  /** The leg filter: which legs are drawn, and how to toggle one. */
  readonly legs?: {
    readonly value: readonly number[]
    readonly onToggle: (legIndex: number) => void
    readonly onClear: () => void
  } | undefined
  /** The socket filter: which addresses are drawn, and how to toggle one. */
  readonly sockets?: {
    readonly value: readonly string[]
    readonly onToggle: (socket: string) => void
    readonly onClear: () => void
  } | undefined
  /** The tag filter: which dialog is lit, and how to pick or drop it. */
  readonly tag?: {
    readonly value: string | undefined
    readonly onSelect: (tag: string) => void
    readonly onClear: () => void
  } | undefined
  /** The legs this ladder drew, where it drew a window of them, and how to move it. */
  readonly window?: DrawnWindow | undefined
}

export const FlowsPane = (
  { layout, selected, onSelect, onlySelected, legs, sockets, tag, window }: FlowsPaneProps
): JSX.Element => {
  const scroll = useScrollToRow(layout.rows.find((row) => row.id === selected?.id)?.y)
  // Picking the tag already picked drops it, the way clicking a selected
  // message puts it away: one control, both directions.
  const pickTag = tag === undefined
    ? undefined
    : (next: string): void => (next === tag.value ? tag.onClear() : tag.onSelect(next))
  return (
    <>
      <div className="ladder-actions">
        <CopyText text={() => flowsToText(layout)} />
      </div>
      {onlySelected !== undefined && (
        <label className="source-toggle">
          <input
            type="checkbox"
            checked={onlySelected.value}
            onChange={(event) => onlySelected.onChange(event.target.checked)}
            disabled={!layout.hasSelection}
          />
          only selected legs (off: the capture around them, for the traffic this call met)
        </label>
      )}
      {(legs !== undefined && legs.value.length > 0) && (
        <p className="source-filter">
          drawing only leg {[...legs.value].sort((a, b) => a - b).join(", ")}
          <button type="button" className="copy-ref" onClick={legs.onClear}>every leg</button>
        </p>
      )}
      {(sockets !== undefined && sockets.value.length > 0) && (
        <p className="source-filter">
          drawing only what {sockets.value.join(" and ")} exchanged
          <button type="button" className="copy-ref" onClick={sockets.onClear}>every socket</button>
        </p>
      )}
      {window !== undefined && <WindowLine window={window} />}
      {(tag !== undefined && tag.value !== undefined) && (
        <p className="source-filter">
          lit: the messages stating <code>{tag.value}</code>
          <button type="button" className="copy-ref" onClick={tag.onClear}>every tag</button>
        </p>
      )}
      <CaptureLegend
        layout={layout}
        sockets={sockets?.value}
        onToggleSocket={sockets?.onToggle}
        selectedTag={tag?.value}
        onSelectTag={pickTag}
        legs={legs?.value}
        onToggleLeg={legs?.onToggle}
      />
      {layout.warnings.length > 0 && (
        <ul className="warnings">
          {layout.warnings.slice(0, 6).map((warning) => <li key={warning}>{warning}</li>)}
        </ul>
      )}
      {/* Only ever drawn for a SELECTION: a placeholder panel above every
          ladder would hold a third of the viewport open for nothing. */}
      {selected !== undefined && (
        <div className="pane-detail">
          <SipMessageView
            record={capturedRecord(selected)}
            body={selected.message.binary ? bodySegments(selected.message) : undefined}
            label={`capture · leg ${selected.legIndex} msg ${selected.message.msgIndex}` +
              (selected.stepId === undefined ? "" : ` · step ${selected.stepId}`)}
          />
        </div>
      )}
      <div className="ladder-scroll" ref={scroll}>
        <FlowsLadder
          layout={layout}
          selectedId={selected?.id}
          onSelect={onSelect}
          selectedTag={tag?.value}
          onSelectTag={pickTag}
          legs={legs?.value}
          onToggleLeg={legs?.onToggle}
        />
      </div>
    </>
  )
}
