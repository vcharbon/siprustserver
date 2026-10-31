/**
 * The site's `/llms.txt`: what the site is, how a reader gets from a test
 * reference to the page about it, every route of the route table with its
 * query and an overview to try, and where the API states itself. The
 * deployment states the first two; the route map is the table's own.
 */
import { absolute, linksHref, LINKS_PATH, OPENAPI_PATH } from "./overview.ts"
import type { RouteKind } from "./route.ts"
import { ROUTE_KINDS, ROUTES } from "./table.ts"

/** How one kind of test reference reaches its page. */
export interface RefForm {
  /** The reference as a reader holds it, `<name>` for its variable part. */
  readonly ref: string
  /** The page URL it opens, `<name>` as in `ref`. */
  readonly page: string
}

/** What the deployment states of its site. */
export interface Site {
  readonly name: string
  /** What the site is, a line each. */
  readonly summary: ReadonlyArray<string>
  readonly refs: ReadonlyArray<RefForm>
  /** A page URL of this deployment per route kind, in place of the table's sample. */
  readonly examples?: Partial<Readonly<Record<RouteKind, string>>>
}

/** The `/llms.txt` of `site` served at `origin`. */
export const llmsText = (site: Site, origin: string): string => {
  const routes = ROUTE_KINDS.map((kind) => {
    const doc = ROUTES[kind]
    const query = Object.keys(doc.query)
    const example = site.examples?.[kind] ?? doc.example
    return [
      `- ${doc.patterns.map((pattern) => `\`${pattern}\``).join(" or ")}: ${doc.page}`,
      ...(query.length === 0 ? [] : [`  query: ${query.join(", ")}`]),
      `  overview of \`${example}\`: ${linksHref(origin, absolute(origin, example))}`
    ].join("\n")
  })
  return [
    `# ${site.name}`,
    "",
    ...site.summary.map((line) => `> ${line}`),
    "",
    `Every page URL has an overview: \`GET ${absolute(origin, LINKS_PATH)}?url=<page URL, URL-encoded>\`.`,
    "It answers Markdown (JSON with `Accept: application/json`): the page in one line, its query explained,",
    "and absolute links to everything the page shows. A page URL fetched as is answers the browser app,",
    "which names its overview in a `<link rel=\"alternate\">` and a `<noscript>` block.",
    "",
    "## From a test reference to its page",
    "",
    ...site.refs.map((form) => `- ${form.ref}: \`${form.page}\``),
    "",
    "## Pages",
    "",
    ...routes,
    "",
    "## API",
    "",
    `- OpenAPI, one description per endpoint: ${absolute(origin, OPENAPI_PATH)}`,
    ""
  ].join("\n")
}
