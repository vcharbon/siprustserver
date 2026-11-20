/**
 * Why one EXTRACTION failed, typed by the step that failed and, for a child
 * process, by how it ended, then said in one line an operator acts on. The
 * line opens with its step (`read:`, `index:`, `anonymise:`, `internal:`),
 * names the binary and its ending (could not start, exited N, killed by a
 * signal with the cgroup's OOM record, ran but its output was not written),
 * and closes on the head of its stderr, `(no stderr)` where it wrote none.
 * The line is what the ingest records: a failure is never an empty reason.
 */
import { IntoFile, Oom, Said } from "@sip/toolchain"
import type { CliUnavailable, Output } from "@sip/toolchain"
import * as path from "node:path"

/** How a child of the extraction ended. */
export type Ending =
  | { readonly _tag: "Unstarted"; readonly why: string }
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

/** Room for a set refusal naming two paths. */
const STDERR_HEAD = 600

/** What a child said, frames out, as one line. */
const oneLine = (stderr: string): string =>
  Said.said(stderr).slice(0, STDERR_HEAD).replace(/["\n]/g, " ").replace(/\s+/g, " ").trim()

const how = (ending: Ending): string => {
  switch (ending._tag) {
    case "Unstarted":
      return `could not start: ${ending.why}`
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

/** How an `IntoFile.exec` run ended; `undefined` where it exited 0 and its output was written. */
export const endingOfFile = (ended: IntoFile.Ended): Ending | undefined => {
  if (!ended.started) return { _tag: "Unstarted", why: ended.broke ?? "the process did not start" }
  if (ended.signal !== null) return { _tag: "Killed", signal: ended.signal, oom: ended.oom }
  if (ended.exit !== 0) return { _tag: "Exited", code: ended.exit ?? -1 }
  if (ended.broke !== undefined) return { _tag: "Unwritten", why: ended.broke }
  return undefined
}

/** How a runner's run ended; `undefined` where it exited 0. */
export const endingOfRun = (
  ran: { readonly _tag: "Output"; readonly output: Output } | {
    readonly _tag: "Unavailable"
    readonly error: CliUnavailable
    readonly oom: Oom.Evidence
  }
): Ending | undefined => {
  if (ran._tag === "Output") return ran.output.exitCode === 0 ? undefined : { _tag: "Exited", code: ran.output.exitCode }
  const signal = Said.signalOf(ran.error.cause)
  if (signal !== undefined) return { _tag: "Killed", signal, oom: ran.oom }
  return { _tag: "Unstarted", why: Said.messageOf(ran.error.cause) }
}
