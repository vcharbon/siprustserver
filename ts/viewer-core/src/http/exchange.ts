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

/**
 * One request the SUT made and the answer it was served. It crosses JSON as
 * it is: what the exchange did not have is an absent key, never `undefined`,
 * and a body it did not carry is null.
 */
export interface HttpExchange {
  /** Wire order within the run, from 1. */
  readonly ordinal: number
  /** When the request WENT OUT: ms from the start of the run, the recording's own origin. */
  readonly atMs: number
  /** When the server received it: absolute Unix µs; absent where the server stated none. */
  readonly receivedAtUs?: number
  /** µs from the server's receipt to its answer; absent where the server stated none. */
  readonly processUs?: number
  /** Where the request went; absent where the run states no destination. */
  readonly dst?: string
  readonly method: string
  /** Path and query, as the request stated it. */
  readonly path: string
  readonly requestHeaders: readonly HttpHeader[]
  readonly requestBody: unknown
  /** The answer's status; absent where the transport failed instead. */
  readonly status?: number
  readonly responseHeaders: readonly HttpHeader[]
  readonly responseBody: unknown
  /** Why no answer came back; absent where one did. */
  readonly error?: string
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

/** The server's process time as the head states it, in ms. */
const processTime = (processUs: number | undefined): string =>
  processUs === undefined ? "" : ` in ${(processUs / 1000).toFixed(3)} ms`

/** The exchange's one-line head: what was asked, how it was answered, and how fast. */
export const httpExchangeLabel = (exchange: HttpExchange): string =>
  `${exchange.method} ${exchange.path} -> ${exchange.status ?? `no answer (${exchange.error ?? "unstated"})`}` +
  processTime(exchange.processUs)

/** A payload as the export prints it: JSON indented, a string as itself. */
export const httpBodyText = (body: unknown): string =>
  body === undefined || body === null
    ? ""
    : typeof body === "string"
    ? body
    : JSON.stringify(body, null, 2)
