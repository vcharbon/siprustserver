/**
 * The route table and what is read off it: every route kind has an entry
 * whose sample URL parses back to that kind; `/llms.txt` lists every route;
 * a page URL's base overview explains its query and links its JSON
 * endpoints absolutely; the app shell points at the overview.
 */
import { describe, expect, it } from "vitest"
import {
  baseOverview,
  campaignDelta,
  linksHref,
  llmsText,
  overviewToMarkdown,
  PAGE_PREFIXES,
  pageUrlOf,
  parseRoute,
  ROUTE_KINDS,
  ROUTES,
  routeOf,
  shellWithPointer
} from "../src/routes/index.ts"

const ORIGIN = "http://viewer.test:5000"

const SITE = {
  name: "Demo viewer",
  summary: ["Callflow ladders of a demo SUT."],
  refs: [{ ref: "a capture basename `<capture>`", page: "/triage/<capture>" }]
}

describe("the route table", () => {
  it("has an entry per route kind whose sample parses back to it", () => {
    for (const kind of ROUTE_KINDS) {
      const sample = new URL(ROUTES[kind].example, ORIGIN)
      expect(parseRoute(sample.pathname).kind, kind).toBe(kind)
    }
    expect(ROUTE_KINDS.length).toBe(14)
  })

  it("names every page prefix the app answers", () => {
    expect([...PAGE_PREFIXES].sort()).toEqual(
      ["/campaign", "/case", "/compare", "/deltas", "/jobs", "/report", "/rules", "/run", "/sources", "/triage"]
    )
  })
})

describe("llms.txt", () => {
  const text = llmsText(SITE, ORIGIN)

  it("lists every route with its forms, its query and an overview to try", () => {
    for (const kind of ROUTE_KINDS) {
      const doc = ROUTES[kind]
      for (const pattern of doc.patterns) expect(text).toContain(`\`${pattern}\``)
      expect(text).toContain(linksHref(ORIGIN, `${ORIGIN}${doc.example}`))
      for (const name of Object.keys(doc.query)) expect(text).toContain(name)
    }
  })

  it("shows a deployment's own page in place of the table's sample", () => {
    const own = llmsText({ ...SITE, examples: { "triage-study": "/triage/c9.pcap?case=c9-k9" } }, ORIGIN)
    expect(own).toContain(linksHref(ORIGIN, `${ORIGIN}/triage/c9.pcap?case=c9-k9`))
    expect(own).not.toContain(linksHref(ORIGIN, `${ORIGIN}${ROUTES["triage-study"].example}`))
  })

  it("states the site, the reference forms and the OpenAPI location", () => {
    expect(text.startsWith("# Demo viewer\n\n> Callflow ladders of a demo SUT.\n")).toBe(true)
    expect(text).toContain("- a capture basename `<capture>`: `/triage/<capture>`")
    expect(text).toContain(`${ORIGIN}/api/openapi.json`)
    expect(text).toContain(`${ORIGIN}/api/links?url=`)
  })
})

describe("a page URL's base overview", () => {
  it("explains the query and links the page's JSON absolutely", () => {
    const raw = "/triage/capture_demo.pcap.gz?case=capture_demo-k1&lane=sut-a&zz=1"
    const url = pageUrlOf(raw, ORIGIN)
    const read = routeOf(url, raw)
    if (!("route" in read) || url === undefined) throw new Error("expected a route")
    const overview = baseOverview(read.route, url, ORIGIN)
    expect(overview.route).toBe("triage-study")
    expect(overview.query.map((p) => [p.name, p.value])).toEqual([
      ["case", "capture_demo-k1"],
      ["lane", "sut-a"],
      ["zz", "1"]
    ])
    expect(overview.query[2]?.meaning).toBe("not read by this page")
    const links = overview.sections.flatMap((s) => s.links.map((l) => l.href))
    expect(links).toContain(`${ORIGIN}/api/triage/capture_demo.pcap.gz?case=capture_demo-k1&lane=sut-a`)
    expect(links).toContain(`${ORIGIN}/api/triage/capture_demo.pcap.gz/history?case=capture_demo-k1`)
    const markdown = overviewToMarkdown(overview)
    expect(markdown).toContain(`Page: ${ORIGIN}/triage/capture_demo.pcap.gz?case=capture_demo-k1&lane=sut-a&zz=1`)
    expect(markdown).toContain("- `lane` = `sut-a`: ")
  })

  it("reads a selector in the path into the endpoint's query", () => {
    const url = new URL("/report/%40run%3A7?lane=sut-a", ORIGIN)
    const read = routeOf(url, url.href)
    if (!("route" in read)) throw new Error("expected a route")
    const links = baseOverview(read.route, url, ORIGIN).sections.flatMap((s) => s.links)
    expect(links[0]?.href).toBe(`${ORIGIN}/api/report?lane=sut-a&run=%40run%3A7`)
  })

  it("refuses a URL that is no page, saying why", () => {
    expect(routeOf(pageUrlOf("http://[x", ORIGIN), "http://[x")).toEqual({ reason: "not a URL: http://[x" })
    expect(routeOf(pageUrlOf("/nowhere/x", ORIGIN), "/nowhere/x")).toEqual({ reason: "no page at /nowhere/x" })
    expect(routeOf(pageUrlOf("/case/%E0%A4%A", ORIGIN), "/case/%E0%A4%A"))
      .toEqual({ reason: "malformed escape in /case/%E0%A4%A" })
    expect(parseRoute("/case/%E0%A4%A")).toEqual({ kind: "unknown", path: "/case/%E0%A4%A" })
    expect(routeOf(pageUrlOf("/api/triage", ORIGIN), "/api/triage"))
      .toEqual({ reason: "/api/triage is an API endpoint, not a page" })
  })
})

describe("the campaign delta", () => {
  it("reads `<run>:<lane>`, and a report's run and lane beside a case", () => {
    expect(campaignDelta(new URLSearchParams("delta=19%3Asut-a"))).toEqual({ run: 19, lane: "sut-a" })
    expect(campaignDelta(new URLSearchParams("run=3&lane=sut-b&case=x"))).toEqual({ run: 3, lane: "sut-b" })
    expect(campaignDelta(new URLSearchParams("run=3&lane=sut-b"))).toBeUndefined()
    expect(campaignDelta(new URLSearchParams("delta=x"))).toBeUndefined()
  })
})

describe("the app shell", () => {
  it("points at the page's overview in the head and in a noscript block", () => {
    const html = "<!doctype html><html><head><title>v</title></head><body><div id=\"root\"></div></body></html>"
    const page = `${ORIGIN}/triage/c.pcap?case=c-k1&lane=a`
    const out = shellWithPointer(html, page, ORIGIN)
    const overview = linksHref(ORIGIN, page).replace(/&/g, "&amp;")
    expect(out).toContain(`<link rel="alternate" type="text/markdown" title="page overview" href="${overview}">`)
    expect(out).toMatch(/<body><\/?\n?<noscript>|<body>\n<noscript>/)
    expect(out).toContain(`${ORIGIN}/llms.txt`)
    expect(out).toContain("<div id=\"root\"></div>")
  })
})
