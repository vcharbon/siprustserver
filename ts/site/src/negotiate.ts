/**
 * An answer in the form the request accepts: JSON where its `Accept` names
 * `application/json`, else Markdown rendered from the same value.
 */
import { HttpServerResponse, type HttpServerRequest } from "effect/unstable/http"

const MARKDOWN = "text/markdown; charset=utf-8"

/** Whether the request asks for JSON. */
export const wantsJson = (request: HttpServerRequest.HttpServerRequest): boolean =>
  (request.headers["accept"] ?? "").includes("application/json")

/** `value` as JSON or as `markdown(value)`, with `status`. */
export const negotiated = <A>(
  request: HttpServerRequest.HttpServerRequest,
  value: A,
  markdown: (value: A) => string,
  status = 200
): HttpServerResponse.HttpServerResponse =>
  wantsJson(request)
    ? HttpServerResponse.jsonUnsafe(value, { status })
    : HttpServerResponse.text(markdown(value), { status, contentType: MARKDOWN })
