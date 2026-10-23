/**
 * What the SUT asked its HTTP service during this run, and what it was
 * served: one block per exchange, in ladder order, payloads folded away.
 *
 * It renders what the run recorded and derives nothing: a payload is the
 * JSON the two sides exchanged, or the text a non-JSON body was.
 */
import { httpBodyText, httpExchangeLabel, type HttpExchange, type HttpHeader } from "@sip/viewer-core/http"
import type { JSX } from "react"

/** The offset a ladder line would carry, from the run's own zero. */
const offset = (atMs: number): string => `+${(atMs / 1000).toFixed(3)}`

const Payload = ({ side, headers, body }: {
  side: string
  headers: readonly HttpHeader[]
  body: unknown
}): JSX.Element | null => {
  const text = httpBodyText(body)
  if (text === "" && headers.length === 0) return null
  return (
    <details className="exchange-payload">
      <summary>{side}{text === "" ? " — headers only" : ` — ${text.length}B`}</summary>
      {headers.length > 0 && (
        <p className="profile-source">
          {headers.map((header) => `${header.name}: ${header.value}`).join(" · ")}
        </p>
      )}
      {text !== "" && <pre className="ledger-answer">{text}</pre>}
    </details>
  )
}

export const Exchange = ({ exchanges, warnings }: {
  exchanges: readonly HttpExchange[]
  warnings?: readonly string[]
}): JSX.Element => (
  <section className="case-profile">
    {(warnings ?? []).map((warning) => <p key={warning} className="error">{warning}</p>)}
    {exchanges.length === 0
      ? (
        <p className="profile-source">
          this bundle records no exchange — the lane recorded none, or the run predates the record
        </p>
      )
      : (
        <ul className="profile-list">
          {exchanges.map((exchange) => (
            <li key={exchange.ordinal} className="ledger-entry">
              <div className="ledger-head">
                <code className="on">#{exchange.ordinal} {offset(exchange.atMs)}</code>
                <code>{httpExchangeLabel(exchange)}</code>
              </div>
              <Payload side="request" headers={exchange.requestHeaders} body={exchange.requestBody} />
              <Payload side="answer" headers={exchange.responseHeaders} body={exchange.responseBody} />
            </li>
          ))}
        </ul>
      )}
  </section>
)
