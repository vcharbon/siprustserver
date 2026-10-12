/** The `+<seconds>.<microseconds>` offset every ladder text line opens with. */

/** Formats a microsecond offset the way `source.txt` prints it. A negative offset reads as +0. */
export const microsecondOffset = (us: number): string => {
  const clamped = Math.max(0, Math.round(us))
  return `+${Math.floor(clamped / 1_000_000)}.${String(clamped % 1_000_000).padStart(6, "0")}`
}
