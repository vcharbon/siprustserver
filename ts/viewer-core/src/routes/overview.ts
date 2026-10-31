/**
 * A page's overview: what one page URL shows, as links a reader with no
 * browser follows — the page in one line, its query explained, then sections
 * of absolute links, each with a label and, where it is a few lines, the data
 * inline. Rendered as Markdown or served as the JSON it is.
 *
 * `baseOverview` reads a page URL through the route table alone: the page
 * line, the query and the JSON endpoints the page reads. A server adds the
 * sections its own data makes worth stating.
 */
import { decodeSegment, type PageRoute, parseRoute, type RouteKind } from "./route.ts"
import { ROUTES, routeDoc } from "./table.ts"

/** One link of an overview: an absolute URL, its label, and short data inline. */
export interface OverviewLink {
  readonly label: string
  readonly href: string
  /** A few lines of the data itself, where they are short enough to need no fetch. */
  readonly inline?: ReadonlyArray<string>
}

/** One section: a heading, lines of short data, and its links. */
export interface OverviewSection {
  readonly heading: string
  readonly lines: ReadonlyArray<string>
  readonly links: ReadonlyArray<OverviewLink>
}

/** One query parameter of the page URL, with what it means to the page. */
export interface OverviewParam {
  readonly name: string
  readonly value: string
  readonly meaning: string
}

export interface Overview {
  /** The page URL, absolute. */
  readonly url: string
  readonly route: RouteKind
  /** What the page is about: the route's line, or the thing it is of. */
  readonly title: string
  /** What the route is, in one line. */
  readonly page: string
  /** What it shows now, a line each; empty where the route has nothing to add. */
  readonly status: ReadonlyArray<string>
  readonly query: ReadonlyArray<OverviewParam>
  readonly sections: ReadonlyArray<OverviewSection>
}

/** A page URL no overview is made of, and why. */
export interface OverviewRefusal {
  readonly reason: string
}

/** The overview endpoint's path: the one place the site names it. */
export const LINKS_PATH = "/api/links"

/** The site's index for a reader with no browser. */
export const LLMS_PATH = "/llms.txt"

/** Where the API states its endpoints. */
export const OPENAPI_PATH = "/api/openapi.json"

/** The overview URL of `pageUrl`, absolute under `origin`. */
export const linksHref = (origin: string, pageUrl: string): string =>
  `${origin}${LINKS_PATH}?url=${encodeURIComponent(pageUrl)}`

/** `path` (site-relative) made absolute under `origin`. */
export const absolute = (origin: string, path: string): string => `${origin}${path}`

/** A page URL read against `origin`: as pasted, absolute or site-relative; undefined where it does not parse. */
export const pageUrlOf = (raw: string, origin: string): URL | undefined => {
  try {
    return new URL(raw.trim(), `${origin}/`)
  } catch {
    return undefined
  }
}

/** The page a URL names, or why none: a URL that does not parse, or a path no route takes. */
export const routeOf = (
  url: URL | undefined,
  raw: string
): { readonly route: PageRoute } | OverviewRefusal => {
  if (url === undefined) return { reason: `not a URL: ${raw}` }
  if (url.pathname.startsWith("/api/")) return { reason: `${url.pathname} is an API endpoint, not a page` }
  if (url.pathname.split("/").some((segment) => decodeSegment(segment) === undefined)) {
    return { reason: `malformed escape in ${url.pathname}` }
  }
  const route = parseRoute(url.pathname)
  return route.kind === "unknown" ? { reason: `no page at ${url.pathname}` } : { route }
}

/** The page's query, each parameter with its meaning, in URL order. */
const queryOf = (kind: RouteKind, query: URLSearchParams): ReadonlyArray<OverviewParam> =>
  [...query].map(([name, value]) => ({
    name,
    value,
    meaning: ROUTES[kind].query[name] ?? "not read by this page"
  }))

/** The overview the route table alone states of `url`: the page line, its query and its JSON endpoints. */
export const baseOverview = (
  route: PageRoute,
  url: URL,
  origin: string
): Overview => {
  const doc = routeDoc(route)
  return {
    url: url.href,
    route: route.kind,
    title: doc.page,
    page: doc.page,
    status: [],
    query: queryOf(route.kind, url.searchParams),
    sections: [{
      heading: "Page data (JSON)",
      lines: [],
      links: doc.api(route, url.searchParams).map((link) => ({ label: link.label, href: absolute(origin, link.path) }))
    }]
  }
}

const bullet = (link: OverviewLink): string =>
  [
    `- ${link.label}: ${link.href}`,
    ...(link.inline ?? []).map((line) => `    ${line}`)
  ].join("\n")

/** The overview as Markdown: title, route line, status, query, then one heading per section. */
export const overviewToMarkdown = (overview: Overview): string => {
  const parts: string[] = [
    `# ${overview.title}`,
    "",
    ...(overview.title === overview.page ? [] : [overview.page, ""]),
    `Page: ${overview.url}`,
    ...(overview.status.length === 0 ? [] : ["", ...overview.status])
  ]
  if (overview.query.length > 0) {
    parts.push("", "## Page query", "", ...overview.query.map((p) => `- \`${p.name}\` = \`${p.value}\`: ${p.meaning}`))
  }
  for (const section of overview.sections) {
    if (section.lines.length === 0 && section.links.length === 0) continue
    parts.push("", `## ${section.heading}`, "")
    if (section.lines.length > 0) parts.push(...section.lines)
    if (section.lines.length > 0 && section.links.length > 0) parts.push("")
    parts.push(...section.links.map(bullet))
  }
  return `${parts.join("\n")}\n`
}

/** A refusal as Markdown, pointing at the site's index. */
export const refusalToMarkdown = (refusal: OverviewRefusal, origin: string): string =>
  `# No overview\n\n${refusal.reason}\n\nThe page URL forms: ${absolute(origin, LLMS_PATH)}\n`
