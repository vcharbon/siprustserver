/**
 * The GENERIC knobs: what a command or a request states nothing about takes —
 * how many cells a batch holds and runs at once, the memory scope the cut and
 * the driver run under, how fast cells start, and whether a sweep ends with
 * the retention pass. A lane may state its own concurrency, batch and pace
 * for its replays (`LaneHosting.LaneDefaults`, resolved in
 * `replay/knob-resolution.ts`); the ingest and the cut take these.
 */

/** Cells per replay batch — one driver boot serves a batch of this many — and captures per cut batch. */
export const DEFAULT_BATCH = 1000

/**
 * Cells at once. What bounds it is the driver's own heap: a parsed pivot, a
 * parsed flows document and the cell's recordings per cell in flight.
 */
export const DEFAULT_CONCURRENCY = 8

/**
 * What the cut and the driver scopes are capped at. Below what the box has
 * free, so the scope's own kill fires first and takes only the batch, never
 * the host's OOM killer.
 */
export const DEFAULT_MEMORY_MAX = "4G"

/**
 * Cell starts per second; `0` means as fast as concurrency slots free. A lane
 * whose SUT is shared states its own ceiling (`LaneHosting`); the knob is how
 * a session goes slower still.
 */
export const DEFAULT_STARTS_PER_SECOND = 0

/**
 * Whether a sweep ends with the retention pass: on. It drops only what a
 * replay remakes, so turning it off is how a session keeps a tree exactly as
 * the sweep left it.
 */
export const DEFAULT_CLEANUP = true
