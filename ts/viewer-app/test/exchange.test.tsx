/**
 * The HTTP exchange pane: one block per exchange in ladder order, head line
 * first, every payload behind a `<details>` that starts closed.
 *
 * The open/close is that element's own — there is no state here to drive — so
 * what this asserts is the markup a click acts on: a `<summary>` naming the
 * side and its size, with the payload already mounted under it.
 */
import type { HttpExchange } from "@sip/viewer-core/http"
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { Exchange } from "../src/components/Exchange.tsx"

const REQUEST = { ref: "r1", target: "bob" }
const ANSWER = { decision: { kind: "ROUTE", to: "sip:bob@example.test" } }

/** One answered consult, one nobody answered. */
const exchanges: readonly HttpExchange[] = [
  {
    ordinal: 1,
    atMs: 3,
    dst: "10.0.0.9:8081",
    method: "POST",
    path: "/route?debug=true",
    requestHeaders: [{ name: "Content-Type", value: "application/json" }],
    requestBody: REQUEST,
    status: 200,
    responseHeaders: [],
    responseBody: ANSWER,
    error: undefined,
    source: {}
  },
  {
    ordinal: 2,
    atMs: 3100,
    dst: "10.0.0.9:8081",
    method: "POST",
    path: "/route/failure",
    requestHeaders: [],
    requestBody: { ref: "r1", cause: "NO_ANSWER" },
    status: undefined,
    responseHeaders: [],
    responseBody: undefined,
    error: "timed out after 3000 ms",
    source: {}
  }
]

const size = (body: unknown): number => JSON.stringify(body, null, 2).length

describe("the exchange pane", () => {
  const markup = renderToStaticMarkup(<Exchange exchanges={exchanges} />)

  it("draws one block per exchange, in ladder order, head line first", () => {
    expect(markup).toContain("#1 +0.003")
    expect(markup).toContain("POST /route?debug=true -&gt; 200")
    expect(markup).toContain("#2 +3.100")
    expect(markup.indexOf("#1 +0.003")).toBeLessThan(markup.indexOf("#2 +3.100"))
  })

  it("states a consult nobody answered as the failure it was", () => {
    expect(markup).toContain("POST /route/failure -&gt; no answer (timed out")
  })

  it("folds every payload away, and mounts it under the summary a click opens", () => {
    // No `<details open>`: every payload starts closed.
    expect(markup).not.toContain("<details open")
    expect(markup.match(/<details class="exchange-payload">/g)).toHaveLength(3)
    expect(markup).toContain(`<summary>request — ${size(REQUEST)}B</summary>`)
    expect(markup).toContain(`<summary>answer — ${size(ANSWER)}B</summary>`)
    // What the click reveals is already there: the payload, whole.
    expect(markup).toContain("&quot;kind&quot;: &quot;ROUTE&quot;")
    expect(markup).toContain("&quot;cause&quot;: &quot;NO_ANSWER&quot;")
  })

  it("says so where a run recorded no exchange at all", () => {
    expect(renderToStaticMarkup(<Exchange exchanges={[]} />)).toContain("records no exchange")
  })
})
