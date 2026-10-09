/**
 * What each leg's scripted actor received, one summary per datagram, read off
 * the run's recording: which step claimed it, its start line and CSeq method,
 * and the subscription state, sipfrag status and session origin it carries.
 * The run-side facts a rule reads beside a confrontation row, which states
 * only the one message it compares.
 */
import type { Bundle } from "@sip/contracts"
import type { Reception } from "./classifier.js"
import { bodyBytesOf, headersInOrderRaw, headerValuesOf, headText, startLineOf } from "./wire.js"

const ORIGIN_LINE = /^o=[^\r\n]*/m
const FRAG_STATUS = /^SIP\/2\.0\s+(\d{3})/

/** The text of a recorded body, within its stated length; empty where it carries none. */
const bodyText = (message: Bundle.RecordedMessage): string => {
  if (message.body === undefined || message.body.len === 0) return ""
  return new TextDecoder("latin1").decode(bodyBytesOf(message).subarray(0, message.body.len))
}

/** One received datagram's summary; undefined where its start line does not parse. */
export const receptionOf = (message: Bundle.RecordedMessage): Reception | undefined => {
  const head = headText(message)
  const start = startLineOf(head)
  if (start === undefined) return undefined
  const headers = headersInOrderRaw(head)
  const cseqMethod = (headerValuesOf(headers, "CSeq")[0] ?? "").trim().split(/\s+/)[1]?.toUpperCase() ?? ""
  const state = headerValuesOf(headers, "Subscription-State")[0]?.split(";")[0]?.trim().toLowerCase()
  const text = bodyText(message)
  const mediaType = (message.body?.content_type ?? "").split(";")[0]!.trim().toLowerCase()
  const frag = mediaType === "message/sipfrag" ? FRAG_STATUS.exec(text.trim()) : null
  const origin = ORIGIN_LINE.exec(text)?.[0]
  return {
    ...(message.step === undefined ? {} : { step: message.step }),
    ...(start.kind === "request" ? { method: start.method } : { status: start.status }),
    cseqMethod,
    ...(state === undefined || state === "" ? {} : { subscriptionState: state }),
    ...(frag === null ? {} : { fragStatus: Number(frag[1]) }),
    ...(origin === undefined ? {} : { origin })
  }
}

/** By leg, every datagram the leg's actor received, retransmissions left out, in wire order. */
export const receptionsOf = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyMap<string, ReadonlyArray<Reception>> =>
  new Map(
    [...recordings]
      .map(([leg, recording]) =>
        [
          leg,
          recording.flatMap((m) => {
            if (m.dir !== "in" || m.repeat_of !== undefined) return []
            const reception = receptionOf(m)
            return reception === undefined ? [] : [reception]
          })
        ] as const
      )
      .filter(([, received]) => received.length > 0)
  )
