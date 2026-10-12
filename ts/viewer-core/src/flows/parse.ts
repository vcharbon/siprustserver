/**
 * Reading a capture flows document — the NON-INTERPRETED source a generated
 * case was cut from.
 *
 * The document's schema is owned by the extractor, so this reader is tolerant
 * by construction: it takes the fields a ladder draws, warns about what it did
 * not understand, and never fails on a field it does not know. Nothing here
 * interprets: a message is a direction, a start line, a time and its bytes.
 */

import { readBody, readPayload, type WireBody, type WireBodyPart, type WirePartHeader } from "../wire/payload.ts"

/** The schema version this reader was written against. */
export const FLOWS_SCHEMA = 5

export interface FlowsMessage {
  /** Index of the leg in the document, the `observed.leg` coordinate. */
  readonly legIndex: number
  /** Index within the leg, the `observed.msg` coordinate. */
  readonly msgIndex: number
  /** Absolute capture timestamp in microseconds. */
  readonly tsUs: number
  readonly src: string
  readonly dst: string
  /** The extractor's retransmission verdict. */
  readonly retx: boolean
  /**
   * The wire as a string: the text arm verbatim, or the head followed by its
   * binary body with one character per byte (latin1), so every byte survives.
   * Empty when the document carried none.
   */
  readonly raw: string
  /** The datagram's length on the wire, in bytes. */
  readonly bytes: number
  /** The body (or, for an opaque datagram, the whole) is not UTF-8: `raw` holds it byte per character. */
  readonly binary: boolean
  /** Characters of `raw` that are the head, as the arm reader measured them. */
  readonly headLength: number
  /** The extractor's body summary, absent where it stated none. */
  readonly body: FlowsBody | undefined
  readonly kind: "request" | "response" | "unknown"
  readonly method: string | undefined
  readonly status: number | undefined
  readonly reason: string | undefined
  readonly uri: string | undefined
  readonly cseqSeq: number | undefined
  readonly cseqMethod: string | undefined
  readonly callId: string
  /** The From-tag the wire carried; absent where the extractor read none. */
  readonly fromTag: string | undefined
  /** The To-tag the wire carried; absent until the answerer has stated one. */
  readonly toTag: string | undefined
  readonly fromUri: string | undefined
  readonly toUri: string | undefined
}

/** One entity header of a MIME part, as the part wrote it. */
export type FlowsPartHeader = WirePartHeader

/** One MIME part, located in the body bytes: `offset`/`len` index the part's content, past its headers. */
export type FlowsBodyPart = WireBodyPart

/** The body's layout as the extractor read it; a multipart body arrives already split. */
export type FlowsBody = WireBody

export interface FlowsLeg {
  readonly index: number
  readonly callId: string
  readonly finalStatus: number | undefined
  readonly terminatedBy: string | undefined
  readonly messages: readonly FlowsMessage[]
}

/**
 * One correlation fact the extractor stated about a group. The kinds are the
 * extractor's vocabulary, so this reader keeps the token and the fields a
 * viewer uses rather than modelling each kind.
 */
export interface FlowsEvidence {
  readonly kind: string
  readonly legs: readonly number[]
  /** `derived_call_id`: the affix the derived Call-ID carries. */
  readonly prefix: string | undefined
  readonly suffix: string | undefined
  /** `shared_header_param`: the header and parameter whose value matched. */
  readonly header: string | undefined
  readonly param: string | undefined
}

export interface FlowsGroup {
  readonly index: number
  readonly legs: readonly number[]
  readonly evidence: readonly FlowsEvidence[]
  readonly t0Us: number | undefined
  readonly finalStatus: number | undefined
}

export interface FlowsDocument {
  readonly schema: number
  /** The legs read, at the indices the document gives them; a window leaves the rest absent. */
  readonly legs: readonly (FlowsLeg | undefined)[]
  readonly groups: readonly FlowsGroup[]
  /** Every message of every leg, ordered by capture time. */
  readonly messages: readonly FlowsMessage[]
  readonly warnings: readonly string[]
}

export type FlowsRead =
  | { readonly ok: true; readonly document: FlowsDocument }
  | { readonly ok: false; readonly reason: string }

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined

const str = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined)
const num = (value: unknown): number | undefined => (typeof value === "number" ? value : undefined)

const legIndices = (value: unknown): readonly number[] =>
  Array.isArray(value) ? value.filter((leg): leg is number => typeof leg === "number") : []

const readMessage = (
  value: unknown,
  legIndex: number,
  msgIndex: number,
  callId: string,
  warnings: string[]
): FlowsMessage | undefined => {
  const message = record(value)
  if (message === undefined) {
    warnings.push(`leg ${legIndex} msg ${msgIndex}: not an object`)
    return undefined
  }
  const summary = record(message["summary"])
  const cseq = record(summary?.["cseq"])
  const from = record(summary?.["from"])
  const to = record(summary?.["to"])
  const kindToken = str(summary?.["kind"])
  return {
    legIndex,
    msgIndex,
    tsUs: num(message["ts_us"]) ?? 0,
    src: str(message["src"]) ?? "?",
    dst: str(message["dst"]) ?? "?",
    retx: message["retx"] === true,
    ...readPayload(message),
    body: readBody(message["body"]),
    kind: kindToken === "request" ? "request" : kindToken === "response" ? "response" : "unknown",
    method: str(summary?.["method"]),
    status: num(summary?.["status"]),
    reason: str(summary?.["reason"]),
    uri: str(summary?.["uri"]),
    cseqSeq: num(cseq?.["seq"]),
    cseqMethod: str(cseq?.["method"]),
    callId,
    fromTag: str(from?.["tag"]),
    toTag: str(to?.["tag"]),
    fromUri: str(from?.["uri"]),
    toUri: str(to?.["uri"])
  }
}

/** Reads a flows document, or says why the value is not one. */
export const parseFlowsDocument = (value: unknown): FlowsRead => {
  const root = record(value)
  if (root === undefined) return { ok: false, reason: "not a JSON object" }
  const legsValue = root["legs"]
  if (!Array.isArray(legsValue)) return { ok: false, reason: "no legs[]: not a flows document" }

  const warnings: string[] = []
  const schema = num(root["schema"]) ?? 0
  if (schema !== FLOWS_SCHEMA) {
    warnings.push(
      `flows schema ${schema || "absent"}, drawn as schema ${FLOWS_SCHEMA}: re-run the extractor's enrich pass for the current shape`
    )
  }

  const legs: FlowsLeg[] = legsValue.map((legValue, legIndex) => {
    const leg = record(legValue) ?? {}
    const callId = str(leg["call_id"]) ?? `leg ${legIndex}`
    const msgs = Array.isArray(leg["msgs"]) ? leg["msgs"] : []
    if (!Array.isArray(leg["msgs"])) warnings.push(`leg ${legIndex}: no msgs[]`)
    const messages = msgs
      .map((msgValue, msgIndex) => readMessage(msgValue, legIndex, msgIndex, callId, warnings))
      .filter((message): message is FlowsMessage => message !== undefined)
    return {
      index: legIndex,
      callId,
      finalStatus: num(leg["final_status"]),
      terminatedBy: str(leg["terminated_by"]),
      messages
    }
  })

  const groupsValue = Array.isArray(root["groups"]) ? root["groups"] : []
  const groups: FlowsGroup[] = groupsValue.map((groupValue, index) => {
    const group = record(groupValue) ?? {}
    return {
      index,
      legs: legIndices(group["legs"]),
      evidence: (Array.isArray(group["evidence"]) ? group["evidence"] : []).flatMap((value) => {
        const entry = record(value)
        const kind = str(entry?.["kind"])
        if (entry === undefined || kind === undefined) return []
        return [{
          kind,
          legs: legIndices(entry["legs"]),
          prefix: str(entry["prefix"]),
          suffix: str(entry["suffix"]),
          header: str(entry["header"]),
          param: str(entry["param"])
        }]
      }),
      t0Us: num(group["t0_us"]),
      finalStatus: num(group["final_status"])
    }
  })

  const messages = legs
    .flatMap((leg) => leg?.messages ?? [])
    .slice()
    .sort((left, right) => left.tsUs - right.tsUs || left.legIndex - right.legIndex || left.msgIndex - right.msgIndex)

  return { ok: true, document: { schema, legs, groups, messages, warnings } }
}

/** The start line a ladder draws for a message. */
export const flowsMessageLabel = (message: FlowsMessage): string => {
  if (message.status !== undefined) {
    return message.reason ? `${message.status} ${message.reason}` : String(message.status)
  }
  return message.method ?? "?"
}
