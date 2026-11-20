/**
 * What a pack states to the campaign runner about its cells (`C`, its
 * extension of the runner's cell): the verb a run is launched by, which the
 * run's lines and holders name, the checks it types itself and its typer
 * of a failed cell's checks, the claims its collectors read of a cell beside
 * its document, the job pre-checks a run asks for by name, the run lock's
 * name, the collector whose line is the log a run keeps of a cell's backend,
 * its lane timers' keys, and what a quick run does not judge.
 */
import type { Cell } from "./cell.js"
import type * as CheckLines from "./check-lines.js"
import type { Typer } from "./judge.js"
import type { TimerKeys } from "./timers.js"

export interface CampaignPack<C extends Cell = Cell> {
  /** The command line's verb a run is launched by (`campaign run`): its stderr lines' prefix, its lock holder's and its junit run property's name. */
  readonly verb: string
  /** The checks the pack types itself, off no lane line (`check-lines.ts`). */
  readonly own: CheckLines.Own
  /** Its typer of a failed cell's checks (`judge.ts`). */
  readonly typer: Typer
  /** The claims its collectors read of `cell` beside its document, by claim name; none for a cell claiming nothing. */
  readonly claimsOf: (cell: C) => Readonly<Record<string, unknown>>
  /** The job pre-checks a run asks for by name, beside those the deployment wires. */
  readonly preChecks: ReadonlyArray<string>
  /** The run lock's name beside the environment's (`exclusive.ts`). */
  readonly lock: string
  /** The collector whose last line of a cell is the log the run keeps of its backend (`record.ts` `logOf`). */
  readonly logCollector: string
  /** The lane-file key of each lane timer its cases name (`timers.ts`). */
  readonly timerKeys: TimerKeys
  /** The collectors its quick level never runs that a quick run's cells still claim of: each cell states it was not given them. */
  readonly quickUnrun: ReadonlyArray<string>
}
