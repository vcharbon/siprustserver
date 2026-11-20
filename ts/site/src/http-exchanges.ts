/**
 * A run bundle's `http.json` — the exchanges the SUT had with the routing
 * service, in wire order — read into the viewer core's `HttpExchanges`.
 *
 * ONE schema, the writer's (the replay's `http` module): the snake_case fields the
 * bundle holds, read by the name it writes and no other. A row this cannot
 * read is reported, never dropped. A body is whatever the file holds — the
 * JSON the service exchanged, or the string a non-JSON body was — and is never
 * re-parsed here; a body the row does not hold is null. What a row does not
 * state is an absent key: the exchanges cross JSON, which holds no undefined.
 */
import { type HttpExchange, type HttpExchanges, type HttpHeader, NO_HTTP_EXCHANGES } from "@sip/viewer-core/http"

const asNumber = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined

const asString = (value: unknown): string | undefined =>
  typeof value === "string" && value.length > 0 ? value : undefined

const headers = (value: unknown): readonly HttpHeader[] => {
  if (!Array.isArray(value)) return []
  return value.flatMap((entry) => {
    const row = typeof entry === "object" && entry !== null ? entry as Record<string, unknown> : undefined
    const name = asString(row?.["name"])
    return name === undefined ? [] : [{ name, value: asString(row?.["value"]) ?? "" }]
  })
}

/**
 * The exchanges a bundle's `http.json` holds. A document that is not an array
 * of rows yields none and says so; ordinals are the file's where it states
 * them, and the row's position otherwise.
 */
export const parseHttpExchanges = (document: unknown): HttpExchanges => {
  if (document === undefined || document === null) return NO_HTTP_EXCHANGES
  if (!Array.isArray(document)) return { exchanges: [], warnings: ["http.json is not a list of exchanges"] }
  const exchanges: HttpExchange[] = []
  const warnings: string[] = []
  document.forEach((entry, index) => {
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      warnings.push(`http.json[${index}]: not an exchange`)
      return
    }
    const row = entry as Record<string, unknown>
    const method = asString(row["method"])
    const path = asString(row["path"])
    if (method === undefined || path === undefined) {
      warnings.push(`http.json[${index}]: no method and path`)
      return
    }
    const dst = asString(row["dst"])
    const status = asNumber(row["status"])
    const error = asString(row["error"])
    const receivedAtUs = asNumber(row["received_at_us"])
    const processUs = asNumber(row["process_us"])
    exchanges.push({
      ordinal: asNumber(row["ordinal"]) ?? index + 1,
      atMs: asNumber(row["at_ms"]) ?? 0,
      ...(receivedAtUs === undefined ? {} : { receivedAtUs }),
      ...(processUs === undefined ? {} : { processUs }),
      ...(dst === undefined ? {} : { dst }),
      method,
      path,
      requestHeaders: headers(row["request_headers"]),
      requestBody: row["request_body"] ?? null,
      ...(status === undefined ? {} : { status }),
      responseHeaders: headers(row["response_headers"]),
      responseBody: row["response_body"] ?? null,
      ...(error === undefined ? {} : { error }),
      source: row
    })
  })
  return { exchanges, warnings }
}
