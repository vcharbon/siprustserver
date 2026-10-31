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
 * Sideways, a ladder whose selected arrow is out of view is scrolled to it;
 * one whose arrow is in view is left alone.
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

/** A scroll box's horizontal geometry. */
export interface BoxWidth {
  readonly scrollLeft: number
  readonly clientWidth: number
  readonly scrollWidth: number
}

/**
 * The scrollLeft that brings the arrow spanning `x1`–`x2` into view, or
 * undefined to leave the box: an arrow wholly in view stays, and so does one
 * wider than the box that already crosses it. Otherwise the arrow is centred.
 */
export const scrollLeftFor = (box: BoxWidth, x1: number, x2: number): number | undefined => {
  const left = Math.min(x1, x2)
  const right = Math.max(x1, x2)
  const viewLeft = box.scrollLeft
  const viewRight = box.scrollLeft + box.clientWidth
  if (left >= viewLeft && right <= viewRight) return undefined
  if (right - left > box.clientWidth && left < viewRight && right > viewLeft) return undefined
  const max = Math.max(0, box.scrollWidth - box.clientWidth)
  return Math.min(max, Math.max(0, (left + right) / 2 - box.clientWidth / 2))
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
  y: number | undefined,
  /** The selected arrow's two ends, where it has them. */
  x?: readonly [number, number] | undefined
): { readonly ref: RefObject<HTMLDivElement | null>; readonly anchorAt: (rowY: number) => void } => {
  const ref = useRef<HTMLDivElement | null>(null)
  const shared = useContext(RowAnchorContext)
  const x1 = x?.[0]
  const x2 = x?.[1]
  useEffect(() => {
    const box = ref.current
    if (box === null || y === undefined) return
    const top = scrollTopFor(box, y, shared?.current)
    const left = x1 === undefined || x2 === undefined ? undefined : scrollLeftFor(box, x1, x2)
    // Instant, not smooth: the reader asked to be taken to a message, and an
    // animation that a re-render interrupts leaves them between two of them.
    if (top !== undefined || left !== undefined) {
      box.scrollTo({ ...(top === undefined ? {} : { top }), ...(left === undefined ? {} : { left }) })
    }
  }, [y, x1, x2, shared])
  const anchorAt = (rowY: number): void => {
    const box = ref.current
    if (shared !== undefined && box !== null) shared.current = anchorOf(box, rowY)
  }
  return { ref, anchorAt }
}
