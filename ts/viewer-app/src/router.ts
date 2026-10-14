/**
 * The whole router. Deep links are the point: every view is reachable by URL,
 * so a ladder can be pasted into a review comment.
 *
 *   /                      the index
 *   /case/<id>             a scenario ladder
 *   /run/<id>              a run-recording ladder
 *   /compare/<a>/<b>       two ladders aligned, each `case:<id>` or `run:<id>`
 *   /compare?a=<s>&b=<s>   two run sets compared call by call (selectors `@env:X`,
 *                          `@run:N`, …); `/compare/<s>/<s>` with two selectors
 *                          reads as this one
 *   /report?run=<s>        one run set's report; `/report/<s>` reads as this one
 *   /triage                the queue, filtered by its query string
 *   /triage/<capture>      one capture's study view
 *   /deltas                the delta sheet of the queue slice its query string names
 *   /rules                 the roster, and what the corpus came to under it
 *   /sources               the source list, filtered by its query string
 *   /jobs                  the jobs, newest first
 *   /jobs/<id>             one job
 *
 * An `<id>` is `<root-id>~<entry-id>`, which the server also uses. A study URL
 * carries the capture BASENAME, so pasting it into a triage launch line works.
 *
 * The query string carries WHERE IN a view the reader is — which lane (`lane`),
 * which slice of the queue (`scope`/`q`/`limit`, one include per chip group
 * under `rule`/`expected`/`refusal`/`outcome`, its excludes repeated under
 * `not-<group>`), which ONE call a study is of (`case`), which replay of a
 * capture is on screen (`cell`) and which step of it (`step`) — so a finding
 * can be linked to, not just the page it was found on.
 */
import { useEffect, useState } from "react"

export type SideRef =
  | { readonly kind: "case"; readonly id: string }
  | { readonly kind: "run"; readonly id: string }

export type Route =
  | { readonly kind: "index" }
  | { readonly kind: "case"; readonly id: string }
  | { readonly kind: "run"; readonly id: string }
  | { readonly kind: "compare"; readonly left: SideRef; readonly right: SideRef }
  /** Two run sets compared; `path` holds the selectors where the path, not the query, stated them. */
  | { readonly kind: "compare-runs"; readonly path?: { readonly a: string; readonly b: string } }
  /** One run set's report; `run` where the path stated the selector. */
  | { readonly kind: "report"; readonly run?: string }
  | { readonly kind: "triage" }
  | { readonly kind: "triage-study"; readonly capture: string }
  | { readonly kind: "deltas" }
  | { readonly kind: "rules" }
  | { readonly kind: "sources" }
  | { readonly kind: "jobs" }
  | { readonly kind: "job"; readonly id: number }
  | { readonly kind: "unknown"; readonly path: string }

export const parseSideRef = (raw: string): SideRef => {
  const decoded = decodeURIComponent(raw)
  const separator = decoded.indexOf(":")
  const kind = separator < 0 ? "case" : decoded.slice(0, separator)
  const id = separator < 0 ? decoded : decoded.slice(separator + 1)
  return kind === "run" ? { kind: "run", id } : { kind: "case", id }
}

export const formatSideRef = (ref: SideRef): string => `${ref.kind}:${encodeURIComponent(ref.id)}`

/**
 * Whether a path segment names a run set rather than a ladder side: a
 * selector term (`@env:X`, `@run:N`, …) or a bare run id (`N`, `runN`).
 */
export const isSelector = (segment: string): boolean => {
  const decoded = decodeURIComponent(segment)
  return decoded.startsWith("@") || /^(run)?\d+$/.test(decoded)
}

export const parseRoute = (pathname: string): Route => {
  const segments = pathname.split("/").filter((segment) => segment.length > 0)
  if (segments.length === 0) return { kind: "index" }
  if (segments[0] === "compare" && segments.length === 1) return { kind: "compare-runs" }
  if (segments[0] === "compare" && segments[1] && segments[2] && isSelector(segments[1]) && isSelector(segments[2])) {
    return {
      kind: "compare-runs",
      path: { a: decodeURIComponent(segments[1]), b: decodeURIComponent(segments[2]) }
    }
  }
  if (segments[0] === "report") {
    return segments[1] ? { kind: "report", run: decodeURIComponent(segments.slice(1).join("/")) } : { kind: "report" }
  }
  if (segments[0] === "case" && segments[1]) {
    return { kind: "case", id: decodeURIComponent(segments[1]) }
  }
  if (segments[0] === "run" && segments[1]) {
    return { kind: "run", id: decodeURIComponent(segments[1]) }
  }
  if (segments[0] === "compare" && segments[1] && segments[2]) {
    return { kind: "compare", left: parseSideRef(segments[1]), right: parseSideRef(segments[2]) }
  }
  if (segments[0] === "rules") return { kind: "rules" }
  if (segments[0] === "deltas") return { kind: "deltas" }
  if (segments[0] === "sources") return { kind: "sources" }
  if (segments[0] === "jobs") {
    const id = Number(segments[1])
    return segments[1] && Number.isInteger(id) ? { kind: "job", id } : { kind: "jobs" }
  }
  if (segments[0] === "triage") {
    return segments[1]
      ? { kind: "triage-study", capture: decodeURIComponent(segments[1]) }
      : { kind: "triage" }
  }
  return { kind: "unknown", path: pathname }
}

export const href = {
  index: () => "/",
  case: (id: string) => `/case/${encodeURIComponent(id)}`,
  run: (id: string) => `/run/${encodeURIComponent(id)}`,
  compare: (left: SideRef, right: SideRef) =>
    `/compare/${formatSideRef(left)}/${formatSideRef(right)}`,
  compareRuns: (params: CompareParams) => `/compare${queryString(params)}`,
  report: (params: ReportParams) => `/report${queryString(params)}`,
  triage: (params: QueueParams = {}) => `/triage${queryString(params)}`,
  triageStudy: (capture: string, params: StudyParams = {}) =>
    `/triage/${encodeURIComponent(capture)}${queryString(params)}`,
  deltas: (params: Omit<QueueParams, "limit"> = {}) => `/deltas${queryString(params)}`,
  rules: (params: RulesParams = {}) => `/rules${queryString(params)}`,
  sources: (params: SourcesParams = {}) => `/sources${queryString(params)}`,
  jobs: () => "/jobs",
  job: (id: number) => `/jobs/${id}`
}

/** Which two run sets a comparison reads, and how much of it. */
export interface CompareParams {
  /** The before side's selector. */
  readonly a?: string | undefined
  /** The after side's selector. */
  readonly b?: string | undefined
  /** The lane a side that pins none is read on; absent = the server's. */
  readonly lane?: string | undefined
  /** `1` lists every compared call, the unchanged ones too. */
  readonly all?: string | undefined
  /** `1` lists only the calls whose two ladders differ, whatever their class. */
  readonly ladderChanged?: string | undefined
}

/** Which run set a report reads. */
export interface ReportParams {
  readonly run?: string | undefined
  readonly lane?: string | undefined
}

/** Which slice of the source list the link points at: its filter bar, as a URL. */
export interface SourcesParams {
  /** `1` = only captures that need ingest or need cut. */
  readonly stale?: string | undefined
  /** Capture-name substring. */
  readonly q?: string | undefined
  /** Rows per page and the page's first row, as text. */
  readonly limit?: string | undefined
  readonly offset?: string | undefined
}

/** Which slice of the roster the link points at: the filter bar, as a URL. */
export interface RulesParams {
  /** Which SUT's runs the post-run counts read; absent = the configured lane. */
  readonly lane?: string | undefined
  /** Rule-name substring. */
  readonly q?: string | undefined
}

/** Which slice of the queue the link points at: the filter bar, as a URL. */
export interface QueueParams {
  /** Which SUT's runs to read; absent = the configured lane. */
  readonly lane?: string | undefined
  /** `red` (the triage default), `all`, `passed`, `failed`, `skipped` or `refused`. */
  readonly scope?: string | undefined
  /** Capture-name substring. */
  readonly q?: string | undefined
  /** Exact main-error rule of a case row: the rule chip. */
  readonly rule?: string | undefined
  /** Exact message a died call waited for: the expected chip. */
  readonly expected?: string | undefined
  /** Exact refusal reason of a refused call: the refusal chip. */
  readonly refusal?: string | undefined
  /** Exact row kind — refused | failed | passed | skipped: the outcome chip. */
  readonly outcome?: string | undefined
  /** One feature the call's document carries (reroute, 18x mode, transfer, MRF …): the feature chip. */
  readonly feature?: string | undefined
  /** The values a group hides, one key per value. */
  readonly "not-rule"?: ReadonlyArray<string> | undefined
  readonly "not-expected"?: ReadonlyArray<string> | undefined
  readonly "not-refusal"?: ReadonlyArray<string> | undefined
  readonly "not-outcome"?: ReadonlyArray<string> | undefined
  readonly "not-feature"?: ReadonlyArray<string> | undefined
  /** Rows to serve; a number as text, because a URL carries text. */
  readonly limit?: string | undefined
}

/** Where in a capture's study the link points: one call, and one of its steps. */
export interface StudyParams {
  /** Which lane's standing the study reports; absent = the configured lane. */
  readonly lane?: string | undefined
  /** The case id: the study is of that ONE call; absent, it is the capture's. */
  readonly case?: string | undefined
  /** `<run>/<cell>`: the exact replay on screen. */
  readonly cell?: string | undefined
  /** The flow step the ladders open focused on. */
  readonly step?: string | undefined
}

/** A list value repeats its key once per entry; a blank or empty value is left out. */
const queryString = (
  params: QueueParams | StudyParams | RulesParams | SourcesParams | CompareParams | ReportParams
): string => {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (Array.isArray(value)) {
      for (const one of value) search.append(key, one)
    } else if (value !== undefined && value !== "") search.set(key, value)
  }
  const text = search.toString()
  return text === "" ? "" : `?${text}`
}

/** The current query string, re-read on every navigation. */
export const useQuery = (): URLSearchParams => {
  const [search, setSearch] = useState(() => window.location.search)
  useEffect(() => {
    const onChange = () => setSearch(window.location.search)
    window.addEventListener("popstate", onChange)
    return () => window.removeEventListener("popstate", onChange)
  }, [])
  return new URLSearchParams(search)
}

/**
 * Rewrites the query string in place. `replaceState`, because moving the
 * selection inside one study is not a navigation — the back button belongs to
 * the pages the reader came through, not to every message they clicked.
 */
export const setQuery = (
  params: QueueParams | StudyParams | RulesParams | SourcesParams | CompareParams | ReportParams
): void => {
  window.history.replaceState({}, "", `${window.location.pathname}${queryString(params)}`)
}

/** The page moved to `path` in place: an old URL form read as its canonical one, the back button untouched. */
export const redirect = (path: string): void => {
  window.history.replaceState({}, "", path)
  window.dispatchEvent(new PopStateEvent("popstate"))
}

export const navigate = (path: string): void => {
  window.history.pushState({}, "", path)
  window.dispatchEvent(new PopStateEvent("popstate"))
}

export const useRoute = (): Route => {
  const [route, setRoute] = useState(() => parseRoute(window.location.pathname))
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(window.location.pathname))
    window.addEventListener("popstate", onChange)
    return () => window.removeEventListener("popstate", onChange)
  }, [])
  return route
}
