/**
 * A request or an answer the API's schema refuses, stated: every route answers
 * it with the schema's own reason as `{ detail }`, never an empty body. A
 * request that does not decode is the asker's (400); an answer that does not
 * encode is the server's (500), and is logged with its route.
 */
import { Effect } from "effect"
import { HttpServerResponse } from "effect/unstable/http"
import { type HttpApiError, HttpApiMiddleware } from "effect/unstable/httpapi"

export class SchemaErrors extends HttpApiMiddleware.Service<SchemaErrors>()("Site.SchemaErrors") {}

/** The schema error kinds that are the server's own answer, not the request. */
const ANSWER_KINDS: ReadonlySet<HttpApiError.HttpApiSchemaError["kind"]> = new Set(["Body", "ResponseHeaders"])

/** Whether a schema error is the server's answer failing to encode. */
export const isAnswer = (error: HttpApiError.HttpApiSchemaError): boolean => ANSWER_KINDS.has(error.kind)

/** What the route says about a schema error: the status and the reason. */
export const statement = (
  error: HttpApiError.HttpApiSchemaError,
  route: string
): { readonly status: number; readonly detail: string } =>
  isAnswer(error)
    ? { status: 500, detail: `${route}: the server's answer did not encode: ${error.cause.message}` }
    : { status: 400, detail: `${route}: the request's ${error.kind.toLowerCase()} did not decode: ${error.cause.message}` }

export const layer = HttpApiMiddleware.layerSchemaErrorTransform(SchemaErrors, (error, { endpoint }) => {
  const { status, detail } = statement(error, endpoint.identifier)
  return (isAnswer(error) ? Effect.logError(detail) : Effect.void).pipe(
    Effect.as(HttpServerResponse.jsonUnsafe({ detail }, { status }))
  )
})

export * as ApiSchemaErrors from "./schema-errors.ts"
