/**
 * What the replay bench reads off its children's logs: the prep a cut
 * recorded, and the sweep and run bundles a replay opened. The lines are the
 * ones `cut.ts` and `replay.ts` print for their operators.
 */

/** The prep the cut's closing line names (`[cut] prep<N> done: …`). */
export const prepOf = (cutLog: string): number | undefined => {
  const match = /^\[cut\] prep(\d+) done:/m.exec(cutLog)
  return match === null ? undefined : Number(match[1])
}

/** One run bundle a replay closed: its capture, its directory name, its cells. */
export interface RunLine {
  readonly capture: string
  readonly run: string
  readonly cells: number
}

/** The sweep a replay opened and every run it closed with cells (`<capture>  run<id>: cells=N …`). */
export const replayOf = (
  replayLog: string
): { readonly sweep: number | undefined; readonly runs: ReadonlyArray<RunLine> } => {
  const sweep = /^\[replay\] sweep (\d+):/m.exec(replayLog)
  const runs: Array<RunLine> = []
  for (const match of replayLog.matchAll(/^(\S+) {2}(run\d+): cells=(\d+)/gm)) {
    runs.push({ capture: match[1]!, run: match[2]!, cells: Number(match[3]) })
  }
  return { sweep: sweep === null ? undefined : Number(sweep[1]), runs }
}
