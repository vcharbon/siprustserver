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
 *
 * The same three slots as `LadderPane`: `.pane-controls`, `.pane-detail`,
 * `.ladder-scroll`.
 */
import {
  bodySegments,
  CaptureLegend,
  FlowsLadder,
  flowsToText,
  type FlowsLayout,
  type FlowsRow
} from "@sip/viewer-core"
import type { JSX, ReactNode } from "react"
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

const Warnings = ({ warnings }: { warnings: readonly string[] }): JSX.Element => (
  <ul className="warnings">
    {warnings.map((warning) => <li key={warning}>{warning}</li>)}
  </ul>
)

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
  /** The copy-as-text button; on by default, off where the host puts its own. */
  readonly copy?: boolean | undefined
  /** The legend and the warnings folded behind summaries; open by default. */
  readonly compact?: boolean | undefined
  /** What the detail slot holds while no row is selected; nothing by default. */
  readonly placeholder?: ReactNode | undefined
}

export const FlowsPane = (
  { layout, selected, onSelect, onlySelected, legs, sockets, tag, window, copy, compact, placeholder }: FlowsPaneProps
): JSX.Element => {
  const drawn = layout.rows.find((row) => row.id === selected?.id)
  const scroll = useScrollToRow(drawn?.y, drawn === undefined ? undefined : [drawn.x1, drawn.x2])
  // Picking the tag already picked drops it, the way clicking a selected
  // message puts it away: one control, both directions.
  const pickTag = tag === undefined
    ? undefined
    : (next: string): void => (next === tag.value ? tag.onClear() : tag.onSelect(next))
  const legend = (
    <CaptureLegend
      layout={layout}
      sockets={sockets?.value}
      onToggleSocket={sockets?.onToggle}
      selectedTag={tag?.value}
      onSelectTag={pickTag}
      legs={legs?.value}
      onToggleLeg={legs?.onToggle}
    />
  )
  return (
    <section className="ladder-pane flows-pane">
      <div className="pane-controls">
        <div className="source-filters">
          {(copy ?? true) && <CopyText text={() => flowsToText(layout)} />}
          {onlySelected !== undefined && (
            <label className="source-toggle" title="off: the capture around this call's legs">
              <input
                type="checkbox"
                checked={onlySelected.value}
                onChange={(event) => onlySelected.onChange(event.target.checked)}
                disabled={!layout.hasSelection}
              />
              selected legs only
            </label>
          )}
          {(legs !== undefined && legs.value.length > 0) && (
            <span className="source-filter">
              leg {[...legs.value].sort((a, b) => a - b).join(", ")}
              <button type="button" className="copy-ref" onClick={legs.onClear}>every leg</button>
            </span>
          )}
          {(sockets !== undefined && sockets.value.length > 0) && (
            <span className="source-filter">
              {sockets.value.join(" ↔ ")}
              <button type="button" className="copy-ref" onClick={sockets.onClear}>every socket</button>
            </span>
          )}
          {(tag !== undefined && tag.value !== undefined) && (
            <span className="source-filter">
              lit <code>{tag.value}</code>
              <button type="button" className="copy-ref" onClick={tag.onClear}>every tag</button>
            </span>
          )}
        </div>
        {window !== undefined && <WindowLine window={window} />}
        {compact === true
          ? (
            <details className="pane-fold capture-legend-fold">
              <summary>legs <span className="count">{layout.legs.length}</span> · sockets <span className="count">{layout.sockets.length}</span></summary>
              {legend}
            </details>
          )
          : legend}
        {layout.warnings.length > 0 && (compact === true
          ? (
            <details className="pane-fold">
              <summary>warnings <span className="count">{layout.warnings.length}</span></summary>
              <Warnings warnings={layout.warnings.slice(0, 6)} />
            </details>
          )
          : <Warnings warnings={layout.warnings.slice(0, 6)} />)}
      </div>
      {selected !== undefined
        ? (
          <div className="pane-detail">
            <SipMessageView
              record={capturedRecord(selected)}
              body={selected.message.binary ? bodySegments(selected.message) : undefined}
              label={`capture · leg ${selected.legIndex} msg ${selected.message.msgIndex}` +
                (selected.stepId === undefined ? "" : ` · step ${selected.stepId}`)}
            />
          </div>
        )
        : placeholder !== undefined && <div className="pane-detail">{placeholder}</div>}
      <div className="ladder-scroll" ref={scroll.ref}>
        <FlowsLadder
          layout={layout}
          selectedId={selected?.id}
          onSelect={(row) => {
            scroll.anchorAt(row.y)
            onSelect(row)
          }}
          selectedTag={tag?.value}
          onSelectTag={pickTag}
          legs={legs?.value}
          onToggleLeg={legs?.onToggle}
        />
      </div>
    </section>
  )
}
