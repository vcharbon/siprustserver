/**
 * Every route's compressible response compressed in the coding the client
 * accepts, API bodies and built files alike. Browsers offer `br` over https
 * only, so a plain-http deployment answers gzip.
 */
import { Effect } from "effect"
import { HttpMiddleware, HttpPlatform, HttpRouter, HttpServerResponse } from "effect/unstable/http"

/** The platform whose compressed response always keeps the content type of the response it compressed. */
const keepingContentType = (platform: HttpPlatform.HttpPlatform["Service"]): HttpPlatform.HttpPlatform["Service"] => ({
  ...platform,
  compression: {
    ...platform.compression,
    compressResponse: (response, algorithm, options) =>
      platform.compression.compressResponse(response, algorithm, options).pipe(
        Effect.map((compressed) => {
          const type = response.headers["content-type"]
          return type === undefined ? compressed : HttpServerResponse.setHeader(compressed, "content-type", type)
        })
      )
  }
})

export const Compression = HttpRouter.middleware(
  (app) => HttpMiddleware.compression()(app).pipe(Effect.updateService(HttpPlatform.HttpPlatform, keepingContentType)),
  { global: true }
)
