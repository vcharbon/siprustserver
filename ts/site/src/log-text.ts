/**
 * A log as a page carries it: its head and its tail, bounded, never the
 * whole file — a hosted-lane log runs to hundreds of megabytes. What is
 * left out is said, with where the whole log is read.
 */
import { Effect } from "effect"
import * as fs from "node:fs"

/** How much of each end of a log a page carries. */
export const END_BYTES = 32 * 1024

/** A log's bounded text and its whole size; `undefined` where it cannot be read. */
export interface Bounded {
  readonly text: string
  readonly bytes: number
}

const readAt = (fd: number, at: number, length: number): string => {
  const buffer = Buffer.alloc(length)
  const read = fs.readSync(fd, buffer, 0, length, at)
  return buffer.subarray(0, read).toString("utf8")
}

/**
 * The log at `file` whole where it is at most twice {@link END_BYTES}, else
 * its head, a line naming the bytes left out and `whole` (where the full log
 * is read), and its tail.
 */
export const bounded = (file: string, whole: string): Effect.Effect<Bounded | undefined> =>
  Effect.sync(() => {
    let fd: number | undefined
    try {
      fd = fs.openSync(file, "r")
      const bytes = fs.fstatSync(fd).size
      if (bytes <= 2 * END_BYTES) return { text: readAt(fd, 0, bytes), bytes }
      const head = readAt(fd, 0, END_BYTES)
      const tail = readAt(fd, bytes - END_BYTES, END_BYTES)
      const left = bytes - 2 * END_BYTES
      return { text: `${head}\n… ${left} byte(s) not shown: the whole log is ${whole} …\n${tail}`, bytes }
    } catch {
      return undefined
    } finally {
      if (fd !== undefined) fs.closeSync(fd)
    }
  })
