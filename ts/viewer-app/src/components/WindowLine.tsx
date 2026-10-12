/**
 * What a ladder says about the legs it drew, and the windows either side.
 *
 * A capture of thousands of calls is read a window at a time, and a pane that
 * did not say so would read as the whole capture.
 */
import { shiftWindow, type FlowsWindow } from "@sip/viewer-core/flows"
import type { JSX } from "react"

export interface DrawnWindow extends FlowsWindow {
  /** Legs the document holds, whatever this window drew. */
  readonly total: number
  /** The case's legs drawn past this window, where the pane opened on them. */
  readonly beyond?: number | undefined
  readonly onWindow: (from: number) => void
}

/**
 * The legs drawn and the document they came out of, and the case's legs drawn
 * past the window where there are any; nothing where it drew them all.
 */
export const windowLine = (window: FlowsWindow, total: number, beyond = 0): string | undefined =>
  window.count >= total
    ? undefined
    : `legs ${window.from}–${window.from + window.count - 1} of ${total}` +
      (beyond === 0 ? "" : `, and the case's ${beyond} leg${beyond === 1 ? "" : "s"} past them`)

/** The line, with the buttons that move the window; nothing where it drew the document. */
export const WindowLine = ({ window }: { window: DrawnWindow }): JSX.Element | null => {
  const line = windowLine(window, window.total, window.beyond ?? 0)
  if (line === undefined) return null
  return (
    <p className="source-filter">
      drawing {line}
      <button
        type="button"
        className="copy-ref"
        disabled={window.from === 0}
        onClick={() => window.onWindow(shiftWindow(window, window.total, -1).from)}
      >
        earlier legs
      </button>
      <button
        type="button"
        className="copy-ref"
        disabled={window.from + window.count >= window.total}
        onClick={() => window.onWindow(shiftWindow(window, window.total, 1).from)}
      >
        later legs
      </button>
    </p>
  )
}
