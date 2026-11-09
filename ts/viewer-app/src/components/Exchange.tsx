/**
 * What the SUT asked its HTTP service during this run, and what it was
 * served: one block per exchange, in ladder order, payloads folded away.
 *
 * It renders what the run recorded and derives nothing: a payload is the
 * JSON the two sides exchanged, or the text a non-JSON body was. Beside a
 * ladder, each exchange names the message it follows as a link that selects
 * it, and the exchanges after the selected message are marked.
 */
import { httpBodyText, httpExchangeLabel, type HttpExchange, type HttpHeader } from "@sip/viewer-core/http"
import type { JSX } from "react"
import type { MessageRef } from "../exchange-after.ts"

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

/** The ladder an exchange pane sits beside: the message each exchange follows, the one selected. */
export interface ExchangeLadder {
  /** By exchange, in `exchanges` order; undefined where it follows no message. */
  readonly after: ReadonlyArray<MessageRef | undefined>
  readonly selectedId: string | undefined
  readonly onSelect: (id: string) => void
}

export const Exchange = ({ exchanges, warnings, ladder }: {
  exchanges: readonly HttpExchange[]
  warnings?: readonly string[]
  ladder?: ExchangeLadder | undefined
}): JSX.Element => (
  <section className="case-profile">
    {(warnings ?? []).map((warning) => <p key={warning} className="error">{warning}</p>)}
    {exchanges.length === 0
      ? (
        <p className="profile-source">
          <span title="the lane recorded none, or the run predates the record">no exchange</span>
        </p>
      )
      : (
        <ul className="profile-list">
          {exchanges.map((exchange, at) => {
            const after = ladder?.after[at]
            const marked = after !== undefined && after.id === ladder?.selectedId
            return (
              <li key={exchange.ordinal} className={marked ? "ledger-entry exchange-marked" : "ledger-entry"}>
                <div className="ledger-head">
                  <code className="on">#{exchange.ordinal} {offset(exchange.atMs)}</code>
                  <code>{httpExchangeLabel(exchange)}</code>
                  {ladder !== undefined && (after === undefined
                    ? <span className="muted">before any message</span>
                    : (
                      <button
                        type="button"
                        className="exchange-after"
                        title="select it in the ladder"
                        onClick={() => ladder.onSelect(after.id)}
                      >
                        after {after.label} ({after.id})
                      </button>
                    ))}
                </div>
                <Payload side="request" headers={exchange.requestHeaders} body={exchange.requestBody} />
                <Payload side="answer" headers={exchange.responseHeaders} body={exchange.responseBody} />
              </li>
            )
          })}
        </ul>
      )}
  </section>
)
