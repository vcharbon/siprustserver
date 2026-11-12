/**
 * The route table: every page the site serves, keyed by route kind — its URL
 * forms, what it shows in one line, each query parameter it reads, and the
 * JSON endpoints its data comes from. The route map of `llms.txt`, the
 * default page overview and the server's page shapes are all read off it, so
 * a route the grammar gains without an entry here does not compile.
 */
import type { PageRoute, Route, RouteKind, RouteOf } from "./route.ts"

/** One JSON endpoint a page reads, as a site-relative path with its query. */
export interface ApiLink {
  readonly label: string
  readonly path: string
}

/** What the site states about one route. */
export interface RouteDoc<R extends Route = Route> {
  /** The page URL forms, `<name>` for a path segment; the first is the canonical one. */
  readonly patterns: ReadonlyArray<string>
  /** What the page shows, in one line. */
  readonly page: string
  /** Each query parameter the page reads, with its meaning in one line. */
  readonly query: Readonly<Record<string, string>>
  /** The JSON endpoints the page reads for `route` at `query`, in reading order. */
  readonly api: (route: R, query: URLSearchParams) => ReadonlyArray<ApiLink>
  /** A page URL of the route: the route map's sample, and what tests open. */
  readonly example: string
}

const segment = encodeURIComponent

/** `path` with the parameters of `query` named in `keys`, repeated keys kept. */
const withQuery = (path: string, query: URLSearchParams, keys: ReadonlyArray<string>): string => {
  const kept = new URLSearchParams()
  for (const [key, value] of query) {
    if (keys.includes(key) && value !== "") kept.append(key, value)
  }
  const text = kept.toString()
  return text === "" ? path : `${path}?${text}`
}

const LANE = "the lane (SUT) whose runs are read; absent, the server's configured lane"

/** The queue's filter words: the triage queue and the delta sheet read the same ones. */
const QUEUE_FILTER: Readonly<Record<string, string>> = {
  lane: LANE,
  scope: "red (default) | all | passed | failed | skipped | refused",
  q: "capture-name substring",
  rule: "exact main-error rule of a row",
  expected: "exact message a died call waited for",
  refusal: "exact refusal reason of a refused call",
  outcome: "exact row kind: refused | failed | passed | skipped",
  feature: "one feature the call's document carries",
  source: "the rule of a source fault the call carries",
  "not-rule": "a rule the queue hides; repeated per value",
  "not-expected": "an expected message the queue hides; repeated per value",
  "not-refusal": "a refusal reason the queue hides; repeated per value",
  "not-outcome": "an outcome the queue hides; repeated per value",
  "not-feature": "a feature the queue hides; repeated per value",
  "not-source": "a source-fault rule the queue hides; repeated per value"
}

const QUEUE_KEYS = Object.keys(QUEUE_FILTER)

const sideApi = (side: { readonly kind: "case" | "run"; readonly id: string }): ApiLink =>
  side.kind === "case"
    ? { label: `case ${side.id}`, path: `/api/cases/${segment(side.id)}` }
    : { label: `run ${side.id}`, path: `/api/runs/${segment(side.id)}` }

export const ROUTES: { readonly [K in RouteKind]: RouteDoc<RouteOf<K>> } = {
  index: {
    patterns: ["/"],
    page: "the index: scenario cases and run recordings",
    query: {},
    api: () => [
      { label: "scenario cases", path: "/api/cases" },
      { label: "run recordings", path: "/api/runs" },
      { label: "server configuration", path: "/api/config" }
    ],
    example: "/"
  },
  case: {
    patterns: ["/case/<root-id>~<entry-id>"],
    page: "one scenario case's ladder",
    query: {},
    api: (route) => [
      { label: "case document", path: `/api/cases/${segment(route.id)}` },
      { label: "case source", path: `/api/cases/${segment(route.id)}/source` }
    ],
    example: "/case/samples~basic.v3"
  },
  run: {
    patterns: ["/run/<root-id>~<entry-id>"],
    page: "one run recording's ladder",
    query: {},
    api: (route) => [{ label: "run recording", path: `/api/runs/${segment(route.id)}` }],
    example: "/run/runs~basic"
  },
  compare: {
    patterns: ["/compare/<case|run>:<id>/<case|run>:<id>"],
    page: "two ladders aligned side by side",
    query: {},
    api: (route) => [sideApi(route.left), sideApi(route.right)],
    example: "/compare/case:samples~basic.v3/run:runs~basic"
  },
  "compare-runs": {
    patterns: ["/compare?a=<selector>&b=<selector>", "/compare/<selector>/<selector>"],
    page: "two run sets compared call by call",
    query: {
      a: "the before side's selector (@develop, @env:X, @run:N, …)",
      b: "the after side's selector",
      lane: "the lane a side that pins none is read on",
      all: "1 lists the unchanged calls too",
      ladderChanged: "1 lists only the calls whose ladders differ"
    },
    api: (route, query) => {
      const search = new URLSearchParams(query)
      if (route.path !== undefined) {
        search.set("a", route.path.a)
        search.set("b", route.path.b)
      }
      return [{
        label: "comparison report",
        path: withQuery("/api/compare", search, ["a", "b", "lane", "all", "ladderChanged"])
      }]
    },
    example: "/compare?a=%40develop&b=%40run%3A2"
  },
  report: {
    patterns: ["/report?run=<selector>", "/report/<selector>"],
    page: "one run set's report",
    query: { run: "the run set's selector (@run:N, @develop, …)", lane: LANE },
    api: (route, query) => {
      const search = new URLSearchParams(query)
      if (route.run !== undefined) search.set("run", route.run)
      return [{ label: "run report", path: withQuery("/api/report", search, ["run", "lane"]) }]
    },
    example: "/report?run=%40run%3A1"
  },
  triage: {
    patterns: ["/triage"],
    page: "the triage queue: one row per call, filtered by the query",
    query: { ...QUEUE_FILTER, limit: "rows to serve" },
    api: (_, query) => [
      { label: "queue rows", path: withQuery("/api/triage", query, [...QUEUE_KEYS, "limit"]) },
      { label: "delta sheet of the slice", path: withQuery("/api/deltas", query, QUEUE_KEYS) }
    ],
    example: "/triage?scope=red"
  },
  "triage-study": {
    patterns: ["/triage/<capture>"],
    page: "a capture's study: its calls; with `case`, one call's replays per lane",
    query: {
      lane: LANE,
      case: "one call of the capture (`<capture>-k…`): the study is of it alone",
      cell: "`<run>/<cell>`: the replay on screen",
      step: "the flow step the ladders open on"
    },
    api: (route, query) => {
      const capture = segment(route.capture)
      const caseId = query.get("case")
      return [
        { label: "study", path: withQuery(`/api/triage/${capture}`, query, ["lane", "case"]) },
        ...(caseId === null
          ? []
          : [{ label: "call history across runs", path: withQuery(`/api/triage/${capture}/history`, query, ["case"]) }]),
        { label: "capture flows envelope", path: `/api/triage/${capture}/flows/envelope` },
        { label: "capture flows legs", path: `/api/triage/${capture}/flows/legs?from=0&count=50` }
      ]
    },
    example: "/triage/capture_demo.pcap.gz?case=capture_demo-k1a2b3c&lane=sut-a"
  },
  deltas: {
    patterns: ["/deltas"],
    page: "the delta sheet of the queue slice the query names",
    query: QUEUE_FILTER,
    api: (_, query) => [{ label: "delta sheet", path: withQuery("/api/deltas", query, QUEUE_KEYS) }],
    example: "/deltas?scope=red"
  },
  rules: {
    patterns: ["/rules"],
    page: "the rule roster and what the corpus came to under each rule",
    query: { lane: LANE, q: "rule-name substring", zeros: "1 lists only rules that never fired" },
    api: (_, query) => [{ label: "rules", path: withQuery("/api/rules", query, ["lane"]) }],
    example: "/rules"
  },
  sources: {
    patterns: ["/sources"],
    page: "the captures of the population: ingest, cut and standing per lane",
    query: {
      stale: "1 lists only captures that need ingest or cut",
      q: "capture-name substring",
      limit: "rows per page",
      offset: "the page's first row"
    },
    api: (_, query) => [{
      label: "source list",
      path: withQuery("/api/sources", query, ["stale", "q", "limit", "offset"])
    }],
    example: "/sources?stale=1"
  },
  pcaps: {
    patterns: ["/pcaps"],
    page: "the files of the pcap root and the inbox: state, last run per lane, replay on a lane",
    query: {},
    api: () => [{ label: "pcap files", path: "/api/pcaps" }],
    example: "/pcaps"
  },
  jobs: {
    patterns: ["/jobs"],
    page: "the jobs, newest first",
    query: { limit: "rows per page", offset: "the page's first row" },
    api: (_, query) => [{ label: "jobs", path: withQuery("/api/jobs", query, ["limit", "offset"]) }],
    example: "/jobs"
  },
  job: {
    patterns: ["/jobs/<id>"],
    page: "one job: its request, progress, log and attachments",
    query: {},
    api: (route) => [
      { label: "job", path: `/api/jobs/${route.id}` },
      { label: "job log", path: `/api/jobs/${route.id}/log` },
      { label: "attachments", path: `/api/attachments?job=${route.id}` },
      { label: "red cells", path: `/api/jobs/${route.id}/cells?outcome=red` }
    ],
    example: "/jobs/1"
  },
  campaign: {
    patterns: ["/campaign", "/campaign/<campaign>"],
    page: "the end-to-end campaigns, or one campaign's runs: results per lane, a case's checks and history",
    query: {
      env: "the environment whose runs are read",
      tab: "`grid` shows the run grid; absent, the failures",
      run: "the run read",
      vs: "the run it is compared with",
      lanes: "the lanes shown, comma-separated",
      fail: "1 shows only failing cases",
      one: "1 shows only cases failing on one lane",
      new: "1 shows only cases failing where the compared run passed",
      q: "case-name or check substring",
      check: "a selected check facet; repeated per value",
      group: "a selected group facet; repeated per value",
      family: "a selected family facet; repeated per value",
      case: "the case whose history is open",
      delta: "`<run>:<lane>`: the open case's result on that run and lane"
    },
    api: (route, query) =>
      route.capture === undefined
        ? [{ label: "campaigns", path: "/api/campaign" }]
        : [{
          label: "campaign runs",
          path: withQuery(`/api/campaign/${segment(route.capture)}`, query, ["env", "run"])
        }],
    example: "/campaign/e2e?case=Family%2Fone+case&delta=2%3Asut-a"
  }
}

/** The table's entry for `route`: the one its kind keys, so its `api` takes `route`. */
export const routeDoc = (route: PageRoute): RouteDoc<PageRoute> =>
  ROUTES[route.kind] as unknown as RouteDoc<PageRoute>

/** Every route kind, in table order. */
export const ROUTE_KINDS = Object.keys(ROUTES) as ReadonlyArray<RouteKind>

/** The path prefixes of every page: what a server answers with the app's shell. */
export const PAGE_PREFIXES: ReadonlyArray<string> = [
  ...new Set(
    ROUTE_KINDS.flatMap((kind) => ROUTES[kind].patterns)
      .map((pattern) => pattern.split(/[?/]/)[1] ?? "")
      .filter((first) => first !== "")
  )
].map((first) => `/${first}`)
