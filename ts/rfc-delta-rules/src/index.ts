/**
 * **@sip/rfc-delta-rules** — the delta rows an RFC alone justifies (ADR-0062
 * item 5), written on `@sip/delta-rules`.
 *
 * | module | what it holds |
 * |---|---|
 * | `stack-owned.ts` | the rows: values the sending stack allocates (Via, Call-ID, CSeq number, tags, Max-Forwards, RSeq, RAck, Content-Length, multipart boundary) |
 * | `facts.ts` | the case facts the rows read, and how each is read off a cell |
 *
 * The deployment places each row in its own sections, adds its lane notes,
 * and extends {@link Facts.RfcFacts} with its own case facts.
 */
export * as Facts from "./facts.js"
export * as StackOwned from "./stack-owned.js"
