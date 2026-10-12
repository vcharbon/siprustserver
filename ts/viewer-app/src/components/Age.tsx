/**
 * An instant, said the two ways a reader needs it: the clock time, and how long
 * ago that was.
 *
 * The age is what answers "did my re-run actually land?", so it ticks on its
 * own — a relative time frozen at first render is the one thing worse than no
 * time at all. The full instant stays on the element's title.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR

/** Recomputed every half minute: the smallest unit shown is the minute. */
const TICK_MS = 30_000

export const ageOf = (at: number, now: number): string => {
  const delta = Math.max(0, now - at)
  if (delta < MINUTE) return "just now"
  if (delta < HOUR) return `${Math.floor(delta / MINUTE)} min ago`
  if (delta < DAY) return `${Math.floor(delta / HOUR)}h ago`
  return `${Math.floor(delta / DAY)}d ago`
}

const clock = (date: Date): string =>
  date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" })

const day = (date: Date): string => date.toLocaleDateString(undefined, { month: "short", day: "numeric" })

export const Age = ({ at, prefix }: { at: string | undefined; prefix?: string }): JSX.Element | null => {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), TICK_MS)
    return () => window.clearInterval(timer)
  }, [])
  if (at === undefined) return null
  const date = new Date(at)
  if (Number.isNaN(date.getTime())) return null
  const sameDay = new Date(now).toDateString() === date.toDateString()
  return (
    <time className="age" dateTime={at} title={date.toString()}>
      {prefix === undefined ? "" : `${prefix} `}
      {sameDay ? clock(date) : `${day(date)} ${clock(date)}`}
      {" · "}
      {ageOf(date.getTime(), now)}
    </time>
  )
}
