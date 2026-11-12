/** The origin a request was addressed to, which every absolute link a page states is built under. */
import type { HttpServerRequest } from "effect/unstable/http"

/** The origin a request was addressed to: its `Host`, behind a proxy its forwarded scheme. */
export const originOf = (request: HttpServerRequest.HttpServerRequest): string => {
  const host = request.headers["x-forwarded-host"] ?? request.headers["host"] ?? "localhost"
  const scheme = request.headers["x-forwarded-proto"] ?? "http"
  return `${scheme}://${host}`
}
