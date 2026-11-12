/**
 * The lines a collector's `collect` answers (ADR-0060 appendix §5), one JSON
 * object each, on its stdout or in {@link OUT_FILE} under `SIPRIG_OUT`: a
 * record joined by key or assigned to a cell, a verification answer, a key
 * added to a cell, an attachment. A line without `cell` answers for the scope
 * itself, where its kind allows that.
 *
 * {@link parse} reads every line or says which one is not a line and why: a
 * collector's typo is a failed collect, never a silently dropped answer.
 */
import { Attachment, Verification } from "@sip/contracts"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import { decoderOf } from "./manifest-file.js"

/** The file under `SIPRIG_OUT` a collector may write its lines to, beside or instead of its stdout. */
export const OUT_FILE = "lines.jsonl"

const Fields = Schema.Record(Schema.String, Schema.Unknown)

/** `{"record": {…}, "key": "<value>"}`: a record the base joins by the manifest's key. */
export const KeyedRecord = Schema.Struct({ record: Fields, key: Schema.String })
export interface KeyedRecord extends Schema.Schema.Type<typeof KeyedRecord> {}

/** `{"record": {…}, "cell": "<id>"}`: a record the collector assigned itself. */
export const AssignedRecord = Schema.Struct({ record: Fields, cell: Schema.String })
export interface AssignedRecord extends Schema.Schema.Type<typeof AssignedRecord> {}

/** An answer a collector may give: `pending` is the base's word for one not given yet. */
export const Answer = Schema.Literals(["pass", "fail", "not-checkable"])

/** `{"verification": "<name>", "cell"?, "answer", "message"?, "detail"?, "informative"?}`. */
export const VerificationLine = Schema.Struct({
  verification: Schema.String,
  cell: Schema.optionalKey(Schema.String),
  answer: Answer,
  message: Schema.optionalKey(Schema.String),
  detail: Schema.optionalKey(Verification.Detail),
  informative: Schema.optionalKey(Schema.Boolean)
})
export interface VerificationLine extends Schema.Schema.Type<typeof VerificationLine> {}

/** `{"key": "<name>", "cell": "<id>", "values": ["…"]}`: a key added to a cell. */
export const KeyLine = Schema.Struct({ key: Schema.String, cell: Schema.String, values: Schema.Array(Schema.String) })
export interface KeyLine extends Schema.Schema.Type<typeof KeyLine> {}

/**
 * `{"attachment": {"kind", "file", "title"?, "name"?}, "cell"?}`: `file` is a
 * path under `SIPRIG_OUT`, or the URL of a `link`; `name` defaults to the
 * collector instance's own.
 */
export const AttachmentLine = Schema.Struct({
  attachment: Schema.Struct({
    name: Schema.optionalKey(Schema.String),
    kind: Attachment.Kind,
    title: Schema.optionalKey(Schema.String),
    file: Schema.String
  }),
  cell: Schema.optionalKey(Schema.String)
})
export interface AttachmentLine extends Schema.Schema.Type<typeof AttachmentLine> {}

/** One line, by its kind. */
export type Line =
  | { readonly _tag: "KeyedRecord"; readonly line: KeyedRecord }
  | { readonly _tag: "AssignedRecord"; readonly line: AssignedRecord }
  | { readonly _tag: "Verification"; readonly line: VerificationLine }
  | { readonly _tag: "Key"; readonly line: KeyLine }
  | { readonly _tag: "Attachment"; readonly line: AttachmentLine }

const decoders = {
  KeyedRecord: decoderOf(KeyedRecord),
  AssignedRecord: decoderOf(AssignedRecord),
  Verification: decoderOf(VerificationLine),
  Key: decoderOf(KeyLine),
  Attachment: decoderOf(AttachmentLine)
} as const

const has = (value: object, key: string): boolean => Object.prototype.hasOwnProperty.call(value, key)

/** The kind an object's keys say it is: the key that only that kind holds. */
const tagOf = (value: object): Line["_tag"] | undefined =>
  has(value, "verification")
    ? "Verification"
    : has(value, "attachment")
    ? "Attachment"
    : has(value, "record")
    ? has(value, "cell") ? "AssignedRecord" : "KeyedRecord"
    : has(value, "values")
    ? "Key"
    : undefined

/** One line's text as a {@link Line}, or why it is none. */
export const lineOf = (text: string): Result.Result<Line, string> => {
  let raw: unknown
  try {
    raw = JSON.parse(text)
  } catch {
    return Result.fail("not JSON")
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return Result.fail("not a JSON object")
  const tag = tagOf(raw)
  if (tag === undefined) return Result.fail("no record, verification, values or attachment key: not a line of a collector")
  const decoded: Result.Result<Line["line"], string> = decoders[tag](raw)
  return Result.map(decoded, (line) => ({ _tag: tag, line }) as Line)
}

/** A line and where it was read: `<origin> line <n>`, `n` counted from 1. */
export interface Placed {
  readonly at: string
  readonly line: Line
}

/** The lines `text` holds, blank ones skipped; or why the first that is none is not, after where it is. */
export const parse = (text: string, origin: string): Result.Result<ReadonlyArray<Placed>, string> => {
  const lines: Array<Placed> = []
  for (const [index, raw] of text.split("\n").entries()) {
    if (raw.trim() === "") continue
    const at = `${origin} line ${index + 1}`
    const read = lineOf(raw)
    if (Result.isFailure(read)) return Result.fail(`${at}: ${read.failure}`)
    lines.push({ at, line: read.success })
  }
  return Result.succeed(lines)
}

/** `line` as the text a collector answers it with, newline included. */
export const textOf = (line: Line): string => `${JSON.stringify(line.line)}\n`
