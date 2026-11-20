/**
 * The names a route may join into a path.
 *
 * A capture, a run and a cell reach the server as SEGMENTS the study listed,
 * never as paths: a separator, a parent reference or an empty name joined
 * unchecked addresses a file outside the tree it was meant to name. Every
 * route that joins one asks here, and nowhere else states the rule.
 */

/** Whether `segment` may be joined into a path as one directory name. */
export const isSegment = (segment: string): boolean =>
  segment !== "" && segment !== "." && segment !== ".." &&
  !segment.includes("/") && !segment.includes("\\") && !segment.includes("\0")

/** Whether every one of `segments` may be joined. */
export const areSegments = (...segments: ReadonlyArray<string>): boolean => segments.every(isSegment)
