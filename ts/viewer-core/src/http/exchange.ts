/**
 * The HTTP side of a run: the exchanges the SUT had with an HTTP service while
 * the run's calls went through it, in wire order.
 *
 * This is the shape the viewer draws, not a file format: a deployment whose
 * lane records such exchanges reads its own file into these values on the
 * server side, and the app and the text export only ever see them. A body is
 * whatever the exchange carried — a parsed JSON value or the string a
 * non-JSON body was.
 */

/** One header line, as the transport carried it. */
export interface HttpHeader {
  readonly name: string
  readonly value: string
}

/** One request the SUT made and the answer it was served. */
export interface HttpExchange {
  /** Wire order within the run, from 1. */
  readonly ordinal: number
  /** When the request WENT OUT: ms from the start of the run, the recording's own origin. */
  readonly atMs: number
  /** Where the request went; absent where the run states no destination. */
  readonly dst: string | undefined
  readonly method: string
  /** Path and query, as the request stated it. */
  readonly path: string
  readonly requestHeaders: readonly HttpHeader[]
  readonly requestBody: unknown
  /** The answer's status; absent where the transport failed instead. */
  readonly status: number | undefined
  readonly responseHeaders: readonly HttpHeader[]
  readonly responseBody: unknown
  /** Why no answer came back, where none did. */
  readonly error: string | undefined
  /** The row as the deployment's file held it, for a side panel. */
  readonly source: unknown
}

/** A run's exchanges, and what of its record did not read as one. */
export interface HttpExchanges {
  readonly exchanges: readonly HttpExchange[]
  readonly warnings: readonly string[]
}

/** A run that recorded no exchange. */
export const NO_HTTP_EXCHANGES: HttpExchanges = { exchanges: [], warnings: [] }

/** The exchange's one-line head: what was asked, and how it was answered. */
export const httpExchangeLabel = (exchange: HttpExchange): string =>
  `${exchange.method} ${exchange.path} -> ${exchange.status ?? `no answer (${exchange.error ?? "unstated"})`}`

/** A payload as the export prints it: JSON indented, a string as itself. */
export const httpBodyText = (body: unknown): string =>
  body === undefined || body === null
    ? ""
    : typeof body === "string"
    ? body
    : JSON.stringify(body, null, 2)
