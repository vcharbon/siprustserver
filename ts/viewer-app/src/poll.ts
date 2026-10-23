/**
 * A held request asked again on a cadence (§6 J3: progress is read, never
 * pushed). The cadence is the caller's to set from what the last answer
 * said — every 2 s while something runs, not at all once nothing does — and
 * the page keeps the last value while the next answer is in flight.
 */
import { useEffect, useState } from "react"
import { type Held, useHeld } from "./held.ts"

/** `useHeld`, re-asked every `everyMs`; `null` polls nothing beyond the first answer. */
export const usePolled = <A>(load: () => Promise<A>, key: string, everyMs: number | null): Held<A> => {
  const [tick, setTick] = useState(0)
  useEffect(() => {
    if (everyMs === null) return
    const timer = window.setInterval(() => setTick((n) => n + 1), everyMs)
    return () => window.clearInterval(timer)
  }, [everyMs])
  return useHeld(load, `${key}#${tick}`)
}
