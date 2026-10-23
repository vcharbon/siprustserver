/**
 * What a dead child SAID, without the frames it said it in.
 *
 * A stack frame states where a failure was raised and never what it was, so a
 * row carrying a few hundred characters of `at <anonymous> (…)` explains
 * nothing to the reader it was put there for. A frame is recognised by its own
 * shape — a `path:line:column` tail — which is what keeps it apart from the
 * `at [...]` a decoder writes INSIDE a message to name the field it refused.
 * Node opens the error's own field dump on its last frame, so that brace is
 * taken with it.
 */

/** `stderr` with every stack frame line taken out. */
export const said = (stderr: string): string =>
  stderr.split("\n").filter((line) => !/^\s*at .+:\d+:\d+\)?\s*\{?\s*$/.test(line)).join("\n")

/** What a thrown value says for itself, without pretending to know its type. */
export const messageOf = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/**
 * The signal a process died of, off the platform's failure to read its exit
 * code (the reason rides in the failure's cause); `undefined` where it died of
 * none.
 */
export const signalOf = (failure: unknown): string | undefined => {
  for (let at: unknown = failure, depth = 0; at !== undefined && at !== null && depth < 8; depth++) {
    const named = /receipt of signal: '([A-Z0-9]+)'/.exec(messageOf(at))?.[1]
    if (named !== undefined) return named
    at = typeof at === "object" ? (at as { readonly cause?: unknown }).cause : undefined
  }
  return undefined
}

export * as Said from "./said.js"
