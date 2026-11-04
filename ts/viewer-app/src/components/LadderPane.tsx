/**
 * A ladder with its legend, its warnings and its selection, in a scroll box.
 *
 * Three slots, in order, each its own element so a host can line them up
 * across panes: `.pane-controls` (copy, legends, warnings, the caller's own
 * controls), `.pane-detail` (the selected message) and `.ladder-scroll`.
 */
import {
  CallLegend,
  Ladder,
  ladderToText,
  LadderLegend,
  type LadderEvent,
  type LadderLayout,
  type LadderMark
} from "@sip/viewer-core"
import type { JSX, ReactNode } from "react"
import { CopyText } from "./CopyText.tsx"
import { useScrollToRow } from "./scroll.ts"

export interface LadderPaneProps {
  readonly layout: LadderLayout
  readonly heading?: string | undefined
  readonly selectedId: string | undefined
  readonly onSelect: (event: LadderEvent) => void
  readonly statusById?: Readonly<Record<string, "matched" | "missing" | "extra">> | undefined
  /** The verdict's findings, on the rows they were made at. */
  readonly marksById?: Readonly<Record<string, LadderMark>> | undefined
  /** The notation panel; on by default, off where two panes would repeat it. */
  readonly legend?: boolean | undefined
  /** The copy-as-text button; on by default, off where the host puts its own. */
  readonly copy?: boolean | undefined
  /** The warnings folded behind a summary; open by default. */
  readonly compact?: boolean | undefined
  /** The host's own controls, after the legends. */
  readonly controls?: ReactNode | undefined
  /**
   * The selected message, rendered ABOVE the ladder. Each ladder shows its
   * own: reading a delta means holding two messages side by side. Absent
   * where nothing is selected and the host asks for no placeholder.
   */
  readonly detail?: ReactNode | undefined
}

export const LadderPane = (
  { layout, heading, selectedId, onSelect, statusById, marksById, legend, copy, compact, controls, detail }: LadderPaneProps
): JSX.Element => {
  const selected = layout.events.find((event) => event.id === selectedId)
  const scroll = useScrollToRow(
    selected?.y,
    selected !== undefined && selected.kind === "message" ? [selected.x1, selected.x2] : undefined
  )
  return (
    <section className="ladder-pane">
      {heading !== undefined && (
        <header className="ladder-pane-head">
          <h2>{heading}</h2>
          {layout.subtitle && <p className="muted">{layout.subtitle}</p>}
        </header>
      )}
      <div className="pane-controls">
        {(copy ?? true) && (
          <div className="ladder-actions">
            <CopyText text={() => ladderToText(layout)} />
          </div>
        )}
        <CallLegend layout={layout} />
        {(legend ?? true) && <LadderLegend layout={layout} />}
        {layout.warnings.length > 0 && (compact === true
          ? (
            <details className="pane-fold">
              <summary>warnings <span className="count">{layout.warnings.length}</span></summary>
              <ul className="warnings">
                {layout.warnings.map((warning) => <li key={warning}>{warning}</li>)}
              </ul>
            </details>
          )
          : (
            <ul className="warnings">
              {layout.warnings.map((warning) => <li key={warning}>{warning}</li>)}
            </ul>
          ))}
        {controls}
      </div>
      {detail !== undefined && <div className="pane-detail">{detail}</div>}
      <div className="ladder-scroll" ref={scroll.ref}>
        <Ladder
          layout={layout}
          selectedId={selectedId}
          onSelect={(event) => {
            scroll.anchorAt(event.y)
            onSelect(event)
          }}
          statusById={statusById}
          marksById={marksById}
        />
      </div>
    </section>
  )
}
