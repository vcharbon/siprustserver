/**
 * The site's URL grammar, pure: a page path read as a `Route`, a `Route` and
 * its place written back as a page URL. The browser app and the server read
 * page URLs through this one module; `table.ts` states what each route is.
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
  | { readonly kind: "pcaps" }
  | { readonly kind: "jobs" }
  | { readonly kind: "job"; readonly id: number }
  | { readonly kind: "campaign"; readonly capture?: string }
  | { readonly kind: "unknown"; readonly path: string }

/** Every route a page URL can name. */
export type RouteKind = Exclude<Route["kind"], "unknown">

/** A route a page URL names: any but `unknown`. */
export type PageRoute = Exclude<Route, { readonly kind: "unknown" }>

/** The route of kind `K`. */
export type RouteOf<K extends RouteKind> = Extract<Route, { readonly kind: K }>

/** A path segment decoded; undefined where its escapes are malformed. */
export const decodeSegment = (segment: string): string | undefined => {
  try {
    return decodeURIComponent(segment)
  } catch {
    return undefined
  }
}

export const parseSideRef = (raw: string): SideRef => {
  const decoded = decodeSegment(raw) ?? raw
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
  const decoded = decodeSegment(segment) ?? segment
  return decoded.startsWith("@") || /^(run)?\d+$/.test(decoded)
}

/** The route `pathname` names; a path no route takes, or whose escapes are malformed, is `unknown`. */
export const parseRoute = (pathname: string): Route => {
  const segments = pathname.split("/").filter((segment) => segment.length > 0)
  if (segments.some((segment) => decodeSegment(segment) === undefined)) return { kind: "unknown", path: pathname }
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
  if (segments[0] === "pcaps") return { kind: "pcaps" }
  if (segments[0] === "jobs") {
    const id = Number(segments[1])
    return segments[1] && Number.isInteger(id) ? { kind: "job", id } : { kind: "jobs" }
  }
  if (segments[0] === "campaign") {
    return segments[1] ? { kind: "campaign", capture: decodeURIComponent(segments[1]) } : { kind: "campaign" }
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
  pcaps: () => "/pcaps",
  jobs: () => "/jobs",
  job: (id: number) => `/jobs/${id}`,
  campaigns: () => "/campaign",
  campaign: (capture: string, params: CampaignParams = {}) =>
    `/campaign/${encodeURIComponent(capture)}${queryString(params)}`
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
  /** The rule of the source fault a call carries, passed or failed: the source chip, information. */
  readonly source?: string | undefined
  /** The values a group hides, one key per value. */
  readonly "not-rule"?: ReadonlyArray<string> | undefined
  readonly "not-expected"?: ReadonlyArray<string> | undefined
  readonly "not-refusal"?: ReadonlyArray<string> | undefined
  readonly "not-outcome"?: ReadonlyArray<string> | undefined
  readonly "not-feature"?: ReadonlyArray<string> | undefined
  readonly "not-source"?: ReadonlyArray<string> | undefined
  /** Rows to serve; a number as text, because a URL carries text. */
  readonly limit?: string | undefined
}

/** Where in a campaign's summary the link points; every value is text, because a URL carries text. */
export interface CampaignParams {
  /** The environment whose runs are read; absent, the server's choice. */
  readonly env?: string | undefined
  /** `grid` for the run grid; absent, the failures. */
  readonly tab?: string | undefined
  /** The run read, and the run it is compared with. */
  readonly run?: string | undefined
  readonly vs?: string | undefined
  /** The lanes read, comma-separated; absent, every lane. */
  readonly lanes?: string | undefined
  readonly fail?: string | undefined
  readonly one?: string | undefined
  readonly new?: string | undefined
  readonly q?: string | undefined
  /** The selected check facets. */
  readonly check?: ReadonlyArray<string> | undefined
  /** The selected group facets. */
  readonly group?: ReadonlyArray<string> | undefined
  /** The selected family facets. */
  readonly family?: ReadonlyArray<string> | undefined
  /** The selected status facets. */
  readonly status?: ReadonlyArray<string> | undefined
  /** The selected verification line facets (`<name> <answer>[ <mark>]`). */
  readonly line?: ReadonlyArray<string> | undefined
  /** The case whose history is open. */
  readonly case?: string | undefined
  /** `<run>:<lane>`: the delta of the open case on screen. */
  readonly delta?: string | undefined
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

/** Any page's place, as its query string carries it. */
export type PageParams =
  | QueueParams
  | StudyParams
  | RulesParams
  | SourcesParams
  | CompareParams
  | ReportParams
  | CampaignParams

/** A list value repeats its key once per entry; a blank or empty value is left out. */
export const queryString = (params: PageParams): string => {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (Array.isArray(value)) {
      for (const one of value) search.append(key, one)
    } else if (value !== undefined && value !== "") search.set(key, value)
  }
  const text = search.toString()
  return text === "" ? "" : `?${text}`
}

const integer = (text: string | null): number | undefined => {
  const value = Number(text)
  return text === null || text === "" || !Number.isInteger(value) ? undefined : value
}

/**
 * The campaign delta a query asks for: `delta=<run>:<lane>`, or the `run` and
 * `lane` a report's link names beside its `case`; absent where it names none.
 */
export const campaignDelta = (
  query: URLSearchParams
): { readonly run: number; readonly lane: string } | undefined => {
  const delta = query.get("delta")
  if (delta !== null) {
    const at = delta.indexOf(":")
    const run = at < 0 ? undefined : integer(delta.slice(0, at))
    return run === undefined ? undefined : { run, lane: delta.slice(at + 1) }
  }
  const run = integer(query.get("run"))
  const lane = query.get("lane")
  return run === undefined || lane === null || query.get("case") === null ? undefined : { run, lane }
}
