/**
 * The recorded datagram a campaign check on an arrival names: the verdict's
 * failure states what came and on which leg, the recording holds the line. A
 * line whose note states the failure's detail is that one; else the first
 * copy no flow step took, else the first copy. A leg the recording does not
 * hold is sought on every leg. An unreadable datagram names no line.
 */
import { recordedEventId, type RecordedMessage } from "@sip/viewer-core"
import type { CampaignCheck } from "./api.ts"

interface Arrival {
  readonly field: string
  readonly seen: string
  readonly leg: unknown
  readonly detail: unknown
  readonly arrived: Readonly<Record<string, unknown>>
}

const isRecord = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

const arrivalsOf = (verdict: unknown): ReadonlyArray<Arrival> => {
  const failures = isRecord(verdict) && Array.isArray(verdict["failures"]) ? verdict["failures"] : []
  return failures.filter(isRecord).flatMap((failure) => {
    const arrived = failure["arrived"]
    return isRecord(arrived)
      ? [{
        field: String(failure["failure"]),
        seen: JSON.stringify(arrived),
        leg: failure["leg"],
        detail: failure["detail"],
        arrived
      }]
      : []
  })
}

const isTheDatagram = (message: RecordedMessage, arrived: Arrival["arrived"]): boolean => {
  if (message.dir !== "in" || message.cseq !== arrived["cseq"]) return false
  if (arrived["kind"] === "request") return message.method === arrived["method"]
  return arrived["kind"] === "response" &&
    message.status === arrived["status"] &&
    message.cseqMethod === arrived["cseq_method"]
}

const lineOf = (arrival: Arrival, messages: ReadonlyArray<RecordedMessage>): RecordedMessage | undefined => {
  const onLeg = messages.some((message) => message.leg === arrival.leg)
  const copies = messages.filter((message) =>
    (!onLeg || message.leg === arrival.leg) && isTheDatagram(message, arrival.arrived)
  )
  return copies.find((message) => message.note !== undefined && message.note === arrival.detail) ??
    copies.find((message) => message.stepId === undefined) ??
    copies[0]
}

/** The id of the datagram each check of `checks` names, in check order; undefined where it names none. */
export const arrivedMessages = (
  checks: ReadonlyArray<Pick<CampaignCheck, "field" | "seen">>,
  verdict: unknown,
  messages: ReadonlyArray<RecordedMessage>
): ReadonlyArray<string | undefined> => {
  const open = [...arrivalsOf(verdict)]
  return checks.map((check) => {
    const at = open.findIndex((arrival) => arrival.field === check.field && arrival.seen === check.seen)
    if (at < 0) return undefined
    const [arrival] = open.splice(at, 1)
    const line = arrival === undefined ? undefined : lineOf(arrival, messages)
    return line === undefined ? undefined : recordedEventId(line)
  })
}
