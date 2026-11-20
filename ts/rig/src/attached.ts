/**
 * An attachment line of a collector or a judge (ADR-0060 appendix §5, §9)
 * made an attachment of its holder: its file copied from the writer's
 * `SIPRIG_OUT` under the holder's `collected/<writer>/`, and listed in the
 * holder's `attachments.jsonl`. A `link` is listed as its URL.
 */
import { Attachment } from "@sip/contracts"
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Lines from "./collector-lines.js"

/** The directory under a holder's where a writer's attached files go, one directory per writer. */
export const COLLECTED_DIR = "collected"

/** Who wrote an attachment line. */
export interface Writer {
  /** File-safe: its directory under the holder's {@link COLLECTED_DIR}, and its attachments' default name. */
  readonly id: string
  /** The attachments' source. */
  readonly source: string
  /** Its `SIPRIG_OUT`, which `file` is relative to. */
  readonly out: string
}

/** Whether `line` attaches a file that `writer` holds: a link, or a file under its `SIPRIG_OUT`; else why not. */
export const faultOf = (writer: Writer, line: Lines.AttachmentLine): string | undefined => {
  const file = line.attachment.file
  if (line.attachment.kind === "link") return undefined
  if (!Attachment.isHeld({ kind: "file", file })) return `attaches ${file}, which is not a path under SIPRIG_OUT`
  const source = path.join(writer.out, file)
  return fs.existsSync(source) && fs.statSync(source).isFile() ? undefined : `attaches ${file}, which is no file under SIPRIG_OUT`
}

/** `line`'s file copied into `dir`'s collected files and listed there; the attachment as listed. */
export const attach = (writer: Writer, dir: string, line: Lines.AttachmentLine): Attachment.Attachment => {
  const { name, kind, title, file } = line.attachment
  const listed = kind === "link" ? file : path.posix.join(COLLECTED_DIR, writer.id, file)
  if (kind !== "link") {
    const target = path.join(dir, listed)
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.copyFileSync(path.join(writer.out, file), target)
  }
  return list(dir, { name: name ?? writer.id, kind, ...(title === undefined ? {} : { title }), file: listed, source: writer.source })
}

/** `attachment`, whose file `dir` already holds, listed in `dir`'s `attachments.jsonl`. */
export const list = (dir: string, attachment: Attachment.Attachment): Attachment.Attachment => {
  fs.mkdirSync(dir, { recursive: true })
  fs.appendFileSync(path.join(dir, Attachment.FILE), Attachment.line(attachment))
  return attachment
}
