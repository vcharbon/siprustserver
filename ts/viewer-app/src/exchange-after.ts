/**
 * The recorded message each HTTP exchange follows: the last ladder message
 * observed at or before the exchange went out, on the recording's own clock
 * (`observed.atUs` beside `atMs`). An exchange before the first message
 * follows none.
 */
import type { HttpExchange } from "@sip/viewer-core/http"
import type { LadderEvent } from "@sip/viewer-core"

/** The message an exchange follows, as a link names it. */
export interface MessageRef {
  readonly id: string
  readonly label: string
}

const observedUs = (event: LadderEvent): number | undefined => {
  for (const decoration of event.decorations) if (decoration.kind === "observed") return decoration.atUs
  return undefined
}

/** For each exchange of `exchanges`, in order, the message it follows among `events`; undefined where none. */
export const exchangesAfter = (
  exchanges: ReadonlyArray<Pick<HttpExchange, "atMs">>,
  events: ReadonlyArray<LadderEvent>
): ReadonlyArray<MessageRef | undefined> => {
  const timed = events.flatMap((event) => {
    if (event.kind !== "message") return []
    const atUs = observedUs(event)
    return atUs === undefined ? [] : [{ atUs, ref: { id: event.id, label: event.label } }]
  })
  return exchanges.map((exchange) => {
    let after: MessageRef | undefined
    for (const message of timed) if (message.atUs / 1000 <= exchange.atMs) after = message.ref
    return after
  })
}
