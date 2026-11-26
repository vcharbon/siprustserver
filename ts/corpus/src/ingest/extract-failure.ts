/**
 * Why one EXTRACTION failed, typed by the step that failed and, for a child
 * process, by how it ended, then said in one line an operator acts on. The
 * line opens with its step (`read:`, `index:`, `anonymise:`, `internal:`),
 * names the binary and its ending (could not be started, exit unreadable,
 * exited N, killed by a signal with the cgroup's OOM record, ran but its
 * output was not written), and closes on the head of its stderr, `(no
 * stderr)` where it wrote none. The line is what the ingest records: a failure
 * is never an empty reason.
 *
 * Its KIND says whose the failure is: the capture's own (`capture`: the read
 * exited on it, or the anonymiser refused it, so the same bytes under the same
 * extractor fail the same way) or the run's (`run`: a child killed, never
 * started, unreadable or unwritten, an index that failed on the run's own
 * document, an internal death), which a retry may not meet again.
 */
import type { Population } from "@sip/store"
import { IntoFile, Oom, Said } from "@sip/toolchain"
import * as path from "node:path"

/** How a child of the extraction ended. */
export type Ending =
  | { readonly _tag: "Unstarted"; readonly why: string }
  | { readonly _tag: "Unawaited"; readonly why: string }
  | { readonly _tag: "Exited"; readonly code: number }
  | { readonly _tag: "Killed"; readonly signal: string; readonly oom: Oom.Evidence | null }
  | { readonly _tag: "Unwritten"; readonly why: string }

/** The extractor's two runs: the raw read of the pcaps and the index of the kept document. */
export type ChildStep = "read" | "index"

export type Failure =
  | {
    readonly _tag: "Child"
    readonly step: ChildStep
    readonly binary: string
    readonly ending: Ending
    readonly stderr: string
  }
  /** The deployment's anonymiser refused; `line` is its own word. */
  | { readonly _tag: "Anonymise"; readonly line: string }
  /** The extraction itself died: a defect or a store write, not a child. */
  | { readonly _tag: "Internal"; readonly message: string }

/** Whose a failure is: the capture's own, or the run's, as the population keeps it. */
export type Kind = Population.IngestFailureKind

/** Room for a set refusal naming two paths. */
const STDERR_HEAD = 600

/** What a child said, frames out, as one line. */
const oneLine = (stderr: string): string =>
  Said.said(stderr).slice(0, STDERR_HEAD).replace(/["\n]/g, " ").replace(/\s+/g, " ").trim()

const how = (ending: Ending): string => {
  switch (ending._tag) {
    case "Unstarted":
      return `could not be started: ${ending.why}`
    case "Unawaited":
      return `ran, and its exit could not be read: ${ending.why}`
    case "Exited":
      return `exited ${ending.code}`
    case "Killed":
      return `was killed by ${ending.signal}${ending.oom === null ? "" : ` (${Oom.words(ending.oom)})`}`
    case "Unwritten":
      return `ran, but its output was not written: ${ending.why}`
  }
}

/** The failure in the one line the ingest records. */
export const lineOf = (failure: Failure): string => {
  switch (failure._tag) {
    case "Child": {
      const said = oneLine(failure.stderr)
      return `${failure.step}: ${path.basename(failure.binary)} ${how(failure.ending)}; ` +
        `stderr: ${said === "" ? "(no stderr)" : said}`
    }
    case "Anonymise":
      return `anonymise: ${failure.line}`
    case "Internal":
      return `internal: the extraction died: ${failure.message}`
  }
}

/** Whose the failure is. */
export const kindOf = (failure: Failure): Kind => {
  switch (failure._tag) {
    case "Anonymise":
      return "capture"
    case "Internal":
      return "run"
    case "Child":
      return failure.step === "read" && failure.ending._tag === "Exited" ? "capture" : "run"
  }
}

/** How an `IntoFile.exec` run ended; `undefined` where it exited 0 and its output was written. */
export const endingOf = (ended: IntoFile.Ended): Ending | undefined => {
  if (ended.brokeAt === "spawn") return { _tag: "Unstarted", why: ended.broke ?? "" }
  if (ended.brokeAt === "await") return { _tag: "Unawaited", why: ended.broke ?? "" }
  if (ended.signal !== null) return { _tag: "Killed", signal: ended.signal, oom: ended.oom }
  if (ended.exit !== null && ended.exit !== 0) return { _tag: "Exited", code: ended.exit }
  if (ended.brokeAt === "output") return { _tag: "Unwritten", why: ended.broke ?? "" }
  return undefined
}
