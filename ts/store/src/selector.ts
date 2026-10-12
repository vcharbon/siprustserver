/**
 * RUN SELECTORS: the one way a run set is named, wherever a command names runs.
 *
 * A selector is one or more terms joined by `/`, and the set is the runs every
 * term admits:
 *
 *   @current        the current population (`current.ts`): its env and
 *                   branch, every tree clean
 *   @env:X          runs made from environment X
 *   @run:N  N  runN one run by id
 *   @sha:X          a clean tree on a commit of any component starting with X
 *                   (7 hex digits at least, one commit only: `RunSets.refusal`)
 *   @lane:L         runs on lane L
 *   @sweep:N        the runs of one sweep
 *   @job:N          the runs one job opened
 *
 * `@env:dev-7/@lane:fake` is env dev-7's runs on the fake lane. A deployment
 * may spell the current term its own way ({@link Spelling}); the term means
 * the same whatever its spelling. Pure: the store turns a parsed selector into
 * rows (`run-sets.ts`).
 */

/** One term of a selector. */
export type Term =
  | { readonly _tag: "current" }
  | { readonly _tag: "env"; readonly env: string }
  | { readonly _tag: "run"; readonly id: number }
  | { readonly _tag: "sha"; readonly prefix: string }
  | { readonly _tag: "lane"; readonly lane: string }
  | { readonly _tag: "sweep"; readonly id: number }
  | { readonly _tag: "job"; readonly id: number }

/** A parsed selector: its text as given, and its terms, every one of which a run must meet. */
export interface Selector {
  readonly text: string
  readonly terms: ReadonlyArray<Term>
}

/** How the current term is written: `@current`, or the name a deployment gives its current population. */
export interface Spelling {
  readonly current: string
}

export const DEFAULT_SPELLING: Spelling = { current: "@current" }

/** The selector forms, as one line a refusal can quote. */
export const formsOf = (spelling: Spelling = DEFAULT_SPELLING): string =>
  `${spelling.current}, @env:X, @run:N (or N, runN), @sha:X (7+ hex), @lane:L, @sweep:N, @job:N, joined by /`

export const FORMS = formsOf()

const ID = /^\d+$/

const idOf = (text: string): number | undefined => (ID.test(text) ? Number(text) : undefined)

const termOf = (text: string, spelling: Spelling): Term | string => {
  if (text === spelling.current) return { _tag: "current" }
  const bare = idOf(text) ?? (text.startsWith("run") ? idOf(text.slice(3)) : undefined)
  if (bare !== undefined) return { _tag: "run", id: bare }
  const at = text.indexOf(":")
  if (!text.startsWith("@") || at < 0) return `"${text}" is no selector term`
  const kind = text.slice(1, at)
  const value = text.slice(at + 1)
  if (value === "") return `"${text}" names no value`
  switch (kind) {
    case "env":
      return { _tag: "env", env: value }
    case "lane":
      return { _tag: "lane", lane: value }
    case "sha":
      return /^[0-9a-f]{7,}$/i.test(value)
        ? { _tag: "sha", prefix: value.toLowerCase() }
        : `"${value}" is no sha prefix of at least 7 hex digits`
    case "run":
    case "sweep":
    case "job": {
      const id = idOf(value)
      return id === undefined ? `"${value}" is no ${kind} id` : { _tag: kind, id }
    }
    default:
      return `"@${kind}" is no selector term`
  }
}

/** A selector's terms, or why the text is none, naming the accepted forms. */
export const parse = (
  text: string,
  spelling: Spelling = DEFAULT_SPELLING
): Selector | { readonly refused: string } => {
  const forms = formsOf(spelling)
  const parts = text.trim().split("/").filter((part) => part !== "")
  if (parts.length === 0) return { refused: `an empty selector: one of ${forms}` }
  const terms: Array<Term> = []
  for (const part of parts) {
    const term = termOf(part, spelling)
    if (typeof term === "string") return { refused: `${term}: one of ${forms}` }
    terms.push(term)
  }
  return { text: text.trim(), terms }
}

export const isRefused = (parsed: Selector | { readonly refused: string }): parsed is { readonly refused: string } =>
  "refused" in parsed

/** Whether the selector pins its runs to a lane of their own: a lane, a run, a sweep or a job. */
export const namesLane = (selector: Selector): boolean =>
  selector.terms.some((term) =>
    term._tag === "lane" || term._tag === "run" || term._tag === "sweep" || term._tag === "job"
  )

/** The selector narrowed to `lane`, unless it pins its lane already. */
export const onLane = (selector: Selector, lane: string): Selector =>
  namesLane(selector) ? selector : { text: `${selector.text}/@lane:${lane}`, terms: [...selector.terms, { _tag: "lane", lane }] }
