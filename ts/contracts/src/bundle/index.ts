/**
 * The **run-bundle contracts**: the record kinds one interpreter run leaves on
 * disk, and that every reader decodes.
 *
 * ```text
 * <run-dir>/
 *   pivot.json              the document that ran, canonically formatted
 *   run-config.json         `RunConfig` — the lane's compiled configuration
 *   recording/<leg>.jsonl   one `RecordedMessage` per line, in wire order
 *   verdict.json            `RunVerdict` — what the run decided, and why
 *   timing.json             `RunTiming` — when it started and when it settled
 *   rfc.json                `RunRfcAudit` — the post-run RFC audit, or that none ran
 * ```
 *
 * Decoders and canonical emitters ride here beside the shapes, so a reader of a
 * bundle never has to remember which parse options a record kind wants.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { format, formatLine } from "../canonical.js"
import { STRICT } from "../strict.js"
import { RunOverlay } from "./overlay.js"
import { RecordedMessage } from "./recording.js"
import { RunRfcAudit } from "./rfc.js"
import { RunConfig } from "./runconfig.js"
import { RunTiming } from "./timing.js"
import { RunVerdict } from "./verdict.js"

export * from "./bindings.js"
export * from "./overlay.js"
export * from "./recording.js"
export * from "./rfc.js"
export * from "./runconfig.js"
export * from "./timing.js"
export * from "./verdict.js"

/**
 * The overlay decodes like every other record here, and it needs to: a driver
 * reads a hand-written one out of a campaign document or a pilot run-spec, and
 * nobody outside this package may build a decoder for a schema this package owns.
 */
export const decodeRunOverlay = Schema.decodeUnknownEffect(RunOverlay, STRICT)
export const decodeRunConfig = Schema.decodeUnknownEffect(RunConfig, STRICT)
export const decodeRunVerdict = Schema.decodeUnknownEffect(RunVerdict, STRICT)
export const decodeRunTiming = Schema.decodeUnknownEffect(RunTiming, STRICT)
export const decodeRecordedMessage = Schema.decodeUnknownEffect(RecordedMessage, STRICT)
export const decodeRunRfcAudit = Schema.decodeUnknownEffect(RunRfcAudit, STRICT)

export const decodeRunOverlaySync = Schema.decodeUnknownSync(RunOverlay, STRICT)
export const decodeRunConfigSync = Schema.decodeUnknownSync(RunConfig, STRICT)
export const decodeRunVerdictSync = Schema.decodeUnknownSync(RunVerdict, STRICT)
export const decodeRunTimingSync = Schema.decodeUnknownSync(RunTiming, STRICT)
export const decodeRecordedMessageSync = Schema.decodeUnknownSync(RecordedMessage, STRICT)
export const decodeRunRfcAuditSync = Schema.decodeUnknownSync(RunRfcAudit, STRICT)

export const emitRunOverlay = (value: RunOverlay): string => format(Schema.encodeUnknownSync(RunOverlay, STRICT)(value))
export const emitRunConfig = (value: RunConfig): string => format(Schema.encodeUnknownSync(RunConfig, STRICT)(value))
export const emitRunVerdict = (value: RunVerdict): string => format(Schema.encodeUnknownSync(RunVerdict, STRICT)(value))
export const emitRunTiming = (value: RunTiming): string => format(Schema.encodeUnknownSync(RunTiming, STRICT)(value))
export const emitRunRfcAudit = (value: RunRfcAudit): string => format(Schema.encodeUnknownSync(RunRfcAudit, STRICT)(value))

/** One recording line, with no newline of its own — the stream owns the line breaks. */
export const emitRecordedMessage = (value: RecordedMessage): string =>
  formatLine(Schema.encodeUnknownSync(RecordedMessage, STRICT)(value))

/** Decode a whole `recording/<leg>.jsonl`, one message per non-empty line. */
export const decodeRecording = (text: string) =>
  Effect.forEach(
    text.split("\n").filter((line) => line.trim().length > 0),
    (line) => decodeRecordedMessage(JSON.parse(line) as unknown)
  )
