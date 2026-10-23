/**
 * Bringing a ladder's selected row into view.
 *
 * A selection made on ANOTHER pane — a confrontation step, a click on the
 * capture — must not leave the reader hunting for the row it picked, so the
 * scroll box is moved to it. A row already on screen is left exactly where it
 * is: re-centring what the reader just clicked reads as the page jumping.
 */
import { useEffect, useRef, type RefObject } from "react"

/** Margin, in px, a row must clear before the box counts it as off-screen. */
const EDGE = 48

export const useScrollToRow = (y: number | undefined): RefObject<HTMLDivElement | null> => {
  const ref = useRef<HTMLDivElement | null>(null)
  useEffect(() => {
    const box = ref.current
    if (box === null || y === undefined) return
    const visible = y >= box.scrollTop + EDGE && y <= box.scrollTop + box.clientHeight - EDGE
    if (visible) return
    // Instant, not smooth: the reader asked to be taken to a message, and an
    // animation that a re-render interrupts leaves them between two of them.
    box.scrollTo({ top: Math.max(0, y - box.clientHeight / 2) })
  }, [y])
  return ref
}
