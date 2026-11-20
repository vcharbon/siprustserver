/**
 * The app's shell at every page URL, pointing at the page's overview
 * (`link rel="alternate"`, `<noscript>`), so a fetch that runs no JavaScript
 * is told where the page's data is.
 */
import { PAGE_PREFIXES, shellWithPointer } from "@sip/viewer-core/routes"
import { Effect, Layer } from "effect"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { HttpRouter, HttpServerRequest, HttpServerResponse } from "effect/unstable/http"
import { ViewerConfig } from "./config.ts"
import { originOf } from "./origin.ts"

/**
 * Every page URL answers the built app's `index.html`, pointing at its own
 * overview. The page forms are the route table's; they are stated rather
 * than sniffed because a case id carries dots (`simple-attempt.v3`), which the
 * static server's own fallback reads as a file. No built app, no page routes.
 */
export const ShellRoutes = Layer.unwrap(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    if (config.staticDir === undefined) return Layer.empty
    const fs = yield* FileSystem
    const path = yield* Path
    const index = path.join(config.staticDir, "index.html")
    const serve = Effect.gen(function*() {
      const request = yield* HttpServerRequest.HttpServerRequest
      const html = yield* fs.readFileString(index).pipe(Effect.orElseSucceed(() => undefined))
      if (html === undefined) return HttpServerResponse.text("no built app", { status: 404 })
      const origin = originOf(request)
      return HttpServerResponse.html(shellWithPointer(html, `${origin}${request.url}`, origin))
    })
    return Layer.mergeAll(
      HttpRouter.add("GET", "/", serve),
      // `<prefix>/*` takes the bare prefix too.
      ...PAGE_PREFIXES.map((prefix) => HttpRouter.add("GET", `${prefix as `/${string}`}/*`, serve))
    )
  })
)
