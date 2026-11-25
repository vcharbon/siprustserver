/**
 * What each leg's scripted actor received, and what it sent, one summary per
 * datagram, read off the run's recording: which step claimed it, its start line
 * and CSeq number and method, its From and Record-Route, and the subscription state,
 * sipfrag status and session origin it carries; and the one order across both
 * directions a rule reads, a BYE ahead of the INVITE's final. The run-side
 * facts a rule reads beside a confrontation row, which states only the one
 * message it compares.
 */
import type { Bundle } from "@sip/contracts"
import type { Reception } from "./classifier.js"
import { items } from "./fold.js"
import { bodyBytesOf, headersInOrderRaw, headerValuesOf, headText, startLineOf } from "./wire.js"

const ORIGIN_LINE = /^o=[^\r\n]*/m
const FRAG_STATUS = /^SIP\/2\.0\s+(\d{3})/
/** A CSeq number: 1*DIGIT (RFC 3261 §20.16), under 2**31 (§8.1.1.5), so ten digits at most. */
const CSEQ_NUMBER = /^\d{1,10}$/

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
  const [cseqNumber = "", method = ""] = (headerValuesOf(headers, "CSeq")[0] ?? "").trim().split(/\s+/)
  const cseqMethod = method.toUpperCase()
  const cseq = CSEQ_NUMBER.test(cseqNumber) ? Number(cseqNumber) : undefined
  const state = headerValuesOf(headers, "Subscription-State")[0]?.split(";")[0]?.trim().toLowerCase()
  const from = headerValuesOf(headers, "From")[0]?.trim()
  const recordRoute = items("record-route", headerValuesOf(headers, "Record-Route"))
  const text = bodyText(message)
  const mediaType = (message.body?.content_type ?? "").split(";")[0]!.trim().toLowerCase()
  const frag = mediaType === "message/sipfrag" ? FRAG_STATUS.exec(text.trim()) : null
  const origin = ORIGIN_LINE.exec(text)?.[0]
  return {
    ...(message.step === undefined ? {} : { step: message.step }),
    ...(start.kind === "request" ? { method: start.method } : { status: start.status }),
    cseqMethod,
    ...(cseq === undefined ? {} : { cseq }),
    ...(state === undefined || state === "" ? {} : { subscriptionState: state }),
    ...(frag === null ? {} : { fragStatus: Number(frag[1]) }),
    ...(origin === undefined ? {} : { origin }),
    ...(from === undefined || from === "" ? {} : { from }),
    ...(recordRoute.length === 0 ? {} : { recordRoute })
  }
}

/** By leg, every datagram the leg's actor received, retransmissions left out, in wire order. */
export const receptionsOf = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyMap<string, ReadonlyArray<Reception>> => summariesOf(recordings, "in")

/** By leg, every datagram the leg's actor sent, retransmissions left out, in wire order. */
export const sentOf = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyMap<string, ReadonlyArray<Reception>> => summariesOf(recordings, "out")

/**
 * The legs whose actor sent a BYE before any final to the INVITE it sent
 * there reached it, in the leg's wire order: a BYE that met the early dialog
 * still standing (RFC 3261 §15). A provisional is no final.
 */
export const byeAheadOfInviteFinal = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlySet<string> =>
  new Set(
    [...recordings].flatMap(([leg, recording]) => {
      const summaries = recording.flatMap((m) => {
        const summary = m.repeat_of === undefined ? receptionOf(m) : undefined
        return summary === undefined ? [] : [{ dir: m.dir, summary }]
      })
      const invited = summaries.some((s) => s.dir === "out" && s.summary.method === "INVITE")
      const bye = summaries.findIndex((s) => s.dir === "out" && s.summary.method === "BYE")
      const final = summaries.findIndex(
        (s) => s.dir === "in" && s.summary.cseqMethod === "INVITE" && (s.summary.status ?? 0) >= 200
      )
      return invited && bye !== -1 && (final === -1 || bye < final) ? [leg] : []
    })
  )

const summariesOf = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>,
  dir: Bundle.Dir
): ReadonlyMap<string, ReadonlyArray<Reception>> =>
  new Map(
    [...recordings]
      .map(([leg, recording]) =>
        [
          leg,
          recording.flatMap((m) => {
            if (m.dir !== dir || m.repeat_of !== undefined) return []
            const reception = receptionOf(m)
            return reception === undefined ? [] : [reception]
          })
        ] as const
      )
      .filter(([, received]) => received.length > 0)
  )
