/**
 * What a triage command line CAME TO: the options the ingest, the cut and the
 * replay share, each bound's arithmetic already applied by the command that
 * read them.
 */
import type { Patience } from "./locks.js"
import type { Paths } from "./paths.js"
import type { Selection } from "./universe.js"

export interface Options {
  readonly paths: Paths
  readonly lane: string
  readonly selection: Selection
  readonly batchSize: number
  readonly concurrency: number
  readonly startsPerSecond: number
  readonly memoryMax: string
  readonly patience: Patience
  readonly dryRun: boolean
  /** The prep a replay reads every capture AS OF; absent pins the highest there is. */
  readonly prep: number | undefined
  /**
   * Whether a SWEEP — more than one capture replayed — ends by running the
   * retention pass; a one-capture invocation never runs it whatever this says.
   */
  readonly cleanup: boolean
  /** Whether an evidence run cuts a named capture again although its standing cut is current. */
  readonly forceRecut: boolean
  /** Whether the cut refuses to run where the census report does not cover every capture. */
  readonly strictCensus?: boolean
}

/**
 * Whether captures that errored fail a replay's exit code: always, except
 * where it replays the known queue (what already fails, whose errors are its
 * own) without asking to be strict.
 */
export const errorsFail = (universe: Selection["universe"], strict: boolean): boolean =>
  universe !== "known" || strict
