/**
 * The study workbench docked under the top bar: its top on the bar's bottom,
 * so the whole block, and every scrollbar in it, is on screen. While it is not
 * docked its boxes do not scroll (CSS on `data-docked`): a wheel over it
 * scrolls the page, and stops the page where the workbench docks.
 */
import { useCallback, useEffect, useRef, useState, type RefObject } from "react"

/** How far, in px, the workbench top may sit from the bar and still count as docked. */
const SLACK = 3

export const isDocked = (top: number, barBottom: number): boolean => Math.abs(top - barBottom) <= SLACK

/**
 * Where a wheel of `delta` px takes the page while the workbench is not
 * docked: the page scrolls by `delta`, never past the scroll position that
 * docks it. Undefined where the wheel moves away from the dock.
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
  readonly docked: boolean
  /** Scrolls the page so the workbench docks. */
  readonly dock: () => void
} => {
  const ref = useRef<HTMLElement | null>(null)
  const [docked, setDocked] = useState(false)
  useEffect(() => {
    const read = (): void => {
      const box = ref.current
      if (box !== null) setDocked(isDocked(box.getBoundingClientRect().top, barBottom()))
    }
    const wheel = (event: WheelEvent): void => {
      const box = ref.current
      if (box === null) return
      const top = box.getBoundingClientRect().top
      if (isDocked(top, barBottom())) return
      const next = wheelTo(window.scrollY, wheelPixels(event), top, barBottom())
      if (next === undefined) return
      event.preventDefault()
      window.scrollTo({ top: next })
    }
    read()
    const box = ref.current
    window.addEventListener("scroll", read, { passive: true })
    window.addEventListener("resize", read)
    box?.addEventListener("wheel", wheel, { passive: false })
    return () => {
      window.removeEventListener("scroll", read)
      window.removeEventListener("resize", read)
      box?.removeEventListener("wheel", wheel)
    }
  }, [])
  const dock = useCallback((): void => {
    ref.current?.scrollIntoView({ block: "start" })
  }, [])
  return { ref, docked, dock }
}
