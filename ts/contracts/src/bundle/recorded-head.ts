/**
 * What a recorded datagram's head states that a reader keys a recorded call
 * on: its start line (a request's method, a response's status), its CSeq
 * method and its Call-ID. It reads the head `Wire` decodes and never
 * validates: a recording holds what crossed the wire, malformations included.
 * A user identity is never read here: the line states it (`identities`).
 */
import * as Wire from "../wire.js"
import type { RecordedMessage } from "./recording.js"

/** The head facts of one recorded datagram; a field the head does not state is absent. */
export interface RecordedHead {
  /** A request's method, upper-cased. */
  readonly method?: string | undefined
  /** A response's status code. */
  readonly status?: number | undefined
  /** The method the CSeq header names, upper-cased. */
  readonly cseqMethod?: string | undefined
  /** The Call-ID header's value (compact form `i` included, RFC 3261 §7.3.3). */
  readonly callId?: string | undefined
}

/** Every header line of a head, continuation lines unfolded, as `[lower-cased name, value]`. */
const headersOf = (head: string): ReadonlyArray<readonly [string, string]> => {
  const out: Array<readonly [string, string]> = []
  for (const line of head.split(/\r?\n/).slice(1)) {
    const last = out[out.length - 1]
    if (/^[ \t]/.test(line) && last !== undefined) {
      out[out.length - 1] = [last[0], `${last[1]} ${line.trim()}`]
      continue
    }
    const colon = line.indexOf(":")
    if (colon > 0) out.push([line.slice(0, colon).trim().toLowerCase(), line.slice(colon + 1).trim()])
  }
  return out
}

/** The head facts `message` states. */
export const recordedHeadOf = (message: RecordedMessage): RecordedHead => {
  const { head } = Wire.headBodyOf(message)
  const start = head.split(/\r?\n/, 1)[0]?.trim() ?? ""
  const headers = headersOf(head)
  const valueOf = (...names: ReadonlyArray<string>) => headers.find(([name]) => names.includes(name))?.[1]
  const response = /^SIP\/2\.0\s+(\d{3})(?:\s|$)/.exec(start)
  const request = /^([A-Za-z]+)\s+\S+\s+SIP\/2\.0$/.exec(start)
  const cseq = /^\d+\s+(\S+)/.exec(valueOf("cseq") ?? "")
  return {
    method: request?.[1]?.toUpperCase(),
    status: response === null ? undefined : Number(response[1]),
    cseqMethod: cseq?.[1]?.toUpperCase(),
    callId: valueOf("call-id", "i")
  }
}
