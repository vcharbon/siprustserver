/**
 * Bringing a ladder's selected row into view, at the same height in every
 * ladder that draws it.
 *
 * A row already where it belongs is left exactly there: re-centring what the
 * reader just clicked reads as the page jumping. Ladders laid out side by side
 * share a `RowAnchor`: the ladder a row was clicked on records the row's
 * offset in its box, and every other ladder scrolls its own row to that
 * offset. A selection made off the ladders (a finding's step) anchors at the
 * centre. Outside a shared anchor, a ladder scrolls only a row off screen.
 */
import { createContext, useContext, useEffect, useRef, type RefObject } from "react"

/** Margin, in px, a row must clear before the box counts it as off-screen. */
const EDGE = 48

/** Where a selected row sits in its box, from the box's top; `centre` for a selection off the ladders. */
export type Anchor = number | "centre"

/** The anchor ladders side by side share; `current` is undefined where no selection set one. */
export interface RowAnchor {
  current: Anchor | undefined
}

export const RowAnchorContext = createContext<RowAnchor | undefined>(undefined)

/** A scroll box's geometry, as the alignment reads it. */
export interface BoxGeometry {
  readonly scrollTop: number
  readonly clientHeight: number
  readonly scrollHeight: number
}

/**
 * The scrollTop that brings row `y` where it belongs, or undefined to leave
 * the box as it is. With an anchor: the row at that offset (kept `EDGE` inside
 * the box), clamped to what the box can scroll, left alone within a pixel. Without one: a visible row
 * stays, a hidden one is centred.
 */
export const scrollTopFor = (box: BoxGeometry, y: number, anchor: Anchor | undefined): number | undefined => {
  const max = Math.max(0, box.scrollHeight - box.clientHeight)
  if (anchor === undefined) {
    const visible = y >= box.scrollTop + EDGE && y <= box.scrollTop + box.clientHeight - EDGE
    return visible ? undefined : Math.min(max, Math.max(0, y - box.clientHeight / 2))
  }
  // An anchor past the box's edges (the box shrank under a detail panel) is
  // pulled back inside, so the row stays on screen.
  const offset = anchor === "centre" || box.clientHeight <= 2 * EDGE
    ? box.clientHeight / 2
    : Math.min(Math.max(anchor, EDGE), box.clientHeight - EDGE)
  const target = Math.min(max, Math.max(0, y - offset))
  return Math.abs(target - box.scrollTop) < 1 ? undefined : target
}

/**
 * The offset a clicked row anchors at: where it is in its box, or the centre
 * where it is not on screen.
 */
export const anchorOf = (box: BoxGeometry, y: number): Anchor => {
  const offset = y - box.scrollTop
  return offset >= 0 && offset <= box.clientHeight ? offset : "centre"
}

/**
 * The scroll box of one ladder, and the click wrapper that records the
 * clicked row's offset as the shared anchor before the selection lands.
 */
export const useScrollToRow = (
  y: number | undefined
): { readonly ref: RefObject<HTMLDivElement | null>; readonly anchorAt: (rowY: number) => void } => {
  const ref = useRef<HTMLDivElement | null>(null)
  const shared = useContext(RowAnchorContext)
  useEffect(() => {
    const box = ref.current
    if (box === null || y === undefined) return
    const top = scrollTopFor(box, y, shared?.current)
    // Instant, not smooth: the reader asked to be taken to a message, and an
    // animation that a re-render interrupts leaves them between two of them.
    if (top !== undefined) box.scrollTo({ top })
  }, [y, shared])
  const anchorAt = (rowY: number): void => {
    const box = ref.current
    if (shared !== undefined && box !== null) shared.current = anchorOf(box, rowY)
  }
  return { ref, anchorAt }
}
