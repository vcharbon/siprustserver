/**
 * One recorded datagram, shown as the SIP message it is.
 *
 * A recording line is JSON carrying a wire message as a string; reading it as
 * JSON puts `\r\n` and quote escapes between the reviewer and the SIP, so the
 * message is rendered on its own — start-line, headers one per line, blank
 * line, body — and the line's bookkeeping (sequence, direction, time, step) is
 * kept beside it rather than interleaved. The verbatim record stays one click
 * away, because the escaping is sometimes exactly what is in question.
 *
 * Nothing is validated here: a malformation is drawn as recorded, and a
 * message this reader cannot split is shown raw with its line breaks restored.
 *
 * The record carries its wire in one of the three arms a capture's message
 * does (`raw` | `head` + `body_b64` | `raw_b64`), read through the core's one
 * payload reader. A body that is bytes rather than text is drawn as segments
 * — framing and text parts as text, a binary part as a hex dump — the ones the
 * caller read out of it, else the ones read here off the record's own layout.
 */
import { ARM_KEYS, bodySegments, bodyText, carriesWire, parseSip, readBody, readPayload, type BodySegment } from "@sip/viewer-core"
import type { JSX } from "react"
import { useState } from "react"

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

/** The record fields that hold the wire message: the three arms, then the pre-decoded `message`. */
const RAW_KEYS: readonly string[] = [...ARM_KEYS, "message"]

/** The wire the record carries, with the segments its body draws as where it is bytes. */
const rawMessage = (
  record: Record<string, unknown>
): { raw: string; binary: boolean; segments: readonly BodySegment[] | undefined } | undefined => {
  if (carriesWire(record)) {
    const wire = readPayload(record)
    if (wire.raw.length === 0) return undefined
    const segments = wire.binary ? bodySegments({ ...wire, body: readBody(record["body"]) }) : undefined
    return { raw: wire.raw, binary: wire.binary, segments }
  }
  const value = record["message"]
  if (typeof value === "string" && value.length > 0) return { raw: value, binary: false, segments: undefined }
  return undefined
}

const metaValue = (value: unknown): string =>
  typeof value === "string" ? value : JSON.stringify(value) ?? "undefined"

/** The record's own fields, minus the message: the line's bookkeeping. */
const Metadata = ({ record }: { record: Record<string, unknown> }): JSX.Element => (
  <div className="sip-meta">
    {Object.entries(record)
      .filter(([key]) => !RAW_KEYS.includes(key))
      .map(([key, value]) => (
        <span key={key} className="sip-meta-item">
          <span className="sip-meta-key">{key}</span>
          <span className="sip-meta-value">{metaValue(value)}</span>
        </span>
      ))}
  </div>
)

/** A binary body, one segment at a time: hex dumps keep their columns, the rest wraps as text. */
const SegmentedBody = ({ segments }: { segments: readonly BodySegment[] }): JSX.Element => (
  <>
    {segments.map((segment, index) => (
      // Segments are positional: the same framing text recurs around each part.
      <span key={index} className={segment.kind === "hex" ? "sip-hex" : segment.kind === "excess" ? "muted" : "sip-body"}>
        {segment.kind === "hex" ? `${segment.text}\n` : segment.kind === "excess" ? `[${segment.text}]\n` : segment.text.replace(/\r\n/g, "\n")}
      </span>
    ))}
  </>
)

export const SipMessageView = (
  { record, label, body: drawn }: {
    record: unknown
    label?: string
    /** The body read out of a binary message, drawn in place of the record's own. */
    body?: readonly BodySegment[] | undefined
  }
): JSX.Element => {
  const [copied, setCopied] = useState(false)
  const source = isRecord(record) ? record : undefined
  const message = source ? rawMessage(source) : undefined
  const raw = message?.raw
  const segments = drawn ?? message?.segments
  const parsed = raw === undefined ? undefined : parseSip(raw)
  const startLine = raw === undefined ? "" : (raw.replace(/^\s+/, "").split(/\r?\n/)[0] ?? "")
  const headers = parsed?.headers ?? []
  const body = parsed?.body?.replace(/\r\n/g, "\n")
  const recordJson = JSON.stringify(record, null, 2) ?? "undefined"
  const copyText = raw === undefined
    ? recordJson
    : segments === undefined
    ? raw
    : raw.slice(0, raw.length - (parsed?.body?.length ?? 0)) + bodyText(segments)

  return (
    <div className="sip-view">
      {source && message && <Metadata record={source} />}

      <div className="sip-view-bar">
        <span>{label ?? "sip message"}</span>
        <button
          type="button"
          onClick={() => {
            void navigator.clipboard?.writeText(copyText)
            setCopied(true)
            window.setTimeout(() => setCopied(false), 1200)
          }}
        >
          {copied ? "copied" : "copy"}
        </button>
      </div>

      {raw === undefined
        ? (
          <>
            <p className="muted">the record carries no wire message — here it is verbatim.</p>
            <pre className="sip-message">{recordJson}</pre>
          </>
        )
        : headers.length === 0
        ? <pre className="sip-message">{raw.replace(/\r\n/g, "\n")}</pre>
        : (
          <pre className="sip-message">
            <span className="sip-start-line">{startLine}</span>
            {"\n"}
            {headers.map((header, index) => (
              // Header order is the wire's, and a message may repeat a name.
              <span key={`${header.name}-${index}`}>
                <span className="sip-header-name">{header.name}</span>
                <span className="sip-header-sep">: </span>
                <span className="sip-header-value">{header.value}</span>
                {"\n"}
              </span>
            ))}
            {segments !== undefined && segments.length > 0
              ? (
                <>
                  {"\n"}
                  <SegmentedBody segments={segments} />
                </>
              )
              : body !== undefined && body.length > 0 && (
                <>
                  {"\n"}
                  <span className="sip-body">{body}</span>
                </>
              )}
          </pre>
        )}

      {raw !== undefined && (
        <details className="sip-record">
          <summary>record, verbatim</summary>
          <pre>{recordJson}</pre>
        </details>
      )}
    </div>
  )
}
