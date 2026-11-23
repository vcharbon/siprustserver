/**
 * The study workbench docked under the top bar: its top on the bar's bottom,
 * so the whole block, and every scrollbar in it, is on screen. Its boxes
 * always scroll; while it is not docked, a mostly vertical wheel over it that
 * moves toward the dock scrolls the page and stops the page where the
 * workbench docks.
 */
import { useCallback, useEffect, useRef, type RefObject } from "react"

/** How far, in px, the workbench top may sit from the bar and still count as docked. */
const SLACK = 3

export const isDocked = (top: number, barBottom: number): boolean => Math.abs(top - barBottom) <= SLACK

/**
 * Where a wheel of `delta` px takes the page while the workbench is not
 * docked: the page scrolls by `delta`, never past the scroll position that
 * docks it. Undefined where the wheel moves away from the dock: the box under
 * the pointer, or the page, takes it.
 */
export const wheelTo = (scrollY: number, delta: number, top: number, barBottom: number): number | undefined => {
  const dockAt = scrollY + (top - barBottom)
  if (delta > 0 && top > barBottom) return Math.min(scrollY + delta, dockAt)
  if (delta < 0 && top < barBottom) return Math.max(scrollY + delta, dockAt)
  return undefined
}

/** A wheel event's vertical travel in px, whatever unit it states. */
const wheelPixels = (event: WheelEvent): number =>
  event.deltaMode === WheelEvent.DOM_DELTA_LINE
    ? event.deltaY * 16
    : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
    ? event.deltaY * window.innerHeight
    : event.deltaY

const barBottom = (): number => document.querySelector(".app-nav")?.getBoundingClientRect().bottom ?? 0

export const useDock = (): {
  readonly ref: RefObject<HTMLElement | null>
  /** Scrolls the page so the workbench docks. */
  readonly dock: () => void
} => {
  const ref = useRef<HTMLElement | null>(null)
  useEffect(() => {
    const wheel = (event: WheelEvent): void => {
      const box = ref.current
      if (box === null || Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return
      const top = box.getBoundingClientRect().top
      if (isDocked(top, barBottom())) return
      const next = wheelTo(window.scrollY, wheelPixels(event), top, barBottom())
      if (next === undefined) return
      event.preventDefault()
      window.scrollTo({ top: next })
    }
    const box = ref.current
    box?.addEventListener("wheel", wheel, { passive: false })
    return () => box?.removeEventListener("wheel", wheel)
  }, [])
  const dock = useCallback((): void => {
    ref.current?.scrollIntoView({ block: "start" })
  }, [])
  return { ref, dock }
}
