/** A ladder with its legend, its warnings and its selection, in a scroll box. */
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
  /**
   * The selected message, rendered ABOVE the ladder and sticky. Each ladder
   * shows its own: reading a delta means holding two messages side by side, and
   * one shared panel at the foot of the page can only ever hold one of them.
   *
   * Absent where nothing is selected — an empty panel would hold a third of the
   * viewport open for a placeholder, on every pane at once.
   */
  readonly detail?: ReactNode | undefined
}

export const LadderPane = (
  { layout, heading, selectedId, onSelect, statusById, marksById, legend, detail }: LadderPaneProps
): JSX.Element => {
  const selected = layout.events.find((event) => event.id === selectedId)
  const scroll = useScrollToRow(selected?.y)
  return (
    <section className="ladder-pane">
      {heading !== undefined && (
        <header className="ladder-pane-head">
          <h2>{heading}</h2>
          {layout.subtitle && <p className="muted">{layout.subtitle}</p>}
        </header>
      )}
      <div className="ladder-actions">
        <CopyText text={() => ladderToText(layout)} />
      </div>
      <CallLegend layout={layout} />
      {(legend ?? true) && <LadderLegend layout={layout} />}
      {layout.warnings.length > 0 && (
        <ul className="warnings">
          {layout.warnings.map((warning) => <li key={warning}>{warning}</li>)}
        </ul>
      )}
      {detail !== undefined && <div className="pane-detail">{detail}</div>}
      <div className="ladder-scroll" ref={scroll}>
        <Ladder
          layout={layout}
          selectedId={selectedId}
          onSelect={onSelect}
          statusById={statusById}
          marksById={marksById}
        />
      </div>
    </section>
  )
}
