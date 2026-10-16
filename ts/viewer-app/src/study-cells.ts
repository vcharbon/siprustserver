/**
 * Which cells a capture's study lists, and which one it opens on.
 *
 * The workdir holds every lane's bundles side by side, and the lanes carry
 * different defects, so a study on one lane reads that lane's cells (a cell
 * whose name states no lane is read either way). The default is the LATEST
 * run's first failing cell, else its first: after a re-run the page shows
 * what just happened. An explicit pick, the reader's or the URL's, wins.
 *
 * A run set aside (`TriageStudySession.aside`) is opened by a pick naming its
 * cell and by nothing else: it is no latest run, it draws no call bar, and it
 * is in the run picker only while it is the one on screen.
 */
import type { TriageStudyCell, TriageStudySession } from "./api.ts"

export interface StudyCellEntry {
  /** `<run>/<cell>`: what the URL's `cell` names. */
  readonly key: string
  readonly session: TriageStudySession
  readonly cell: TriageStudyCell
}

export interface StudyCells {
  /** The cells of the runs not set aside, in run order. */
  readonly cells: ReadonlyArray<StudyCellEntry>
  /** The latest such run's cells: what the call bar is drawn from. */
  readonly latest: ReadonlyArray<StudyCellEntry>
  /** The cell on screen; none where no run but an aside one holds a cell and nothing names it. */
  readonly active: StudyCellEntry | undefined
  /** The run picker's entries: {@link cells}, and the aside cell on screen. */
  readonly picker: ReadonlyArray<StudyCellEntry>
}

/** A cell passed when the run's index says so, else by its interpreter's word (`ok-negative` included). */
export const cellPassed = (cell: TriageStudyCell): boolean =>
  cell.passed ?? (cell.status === "ok" || cell.status === "ok-negative")

/** The CALL a cell replayed: a cell dir is `<case>__<shape>__<lane>`. */
export const caseIdOf = (cellName: string): string => cellName.split("__")[0] ?? cellName

export const studyCells = (
  sessions: ReadonlyArray<TriageStudySession>,
  lane: string | undefined,
  cellKey: string | undefined,
  queryCase: string | undefined
): StudyCells => {
  const entries = sessions.flatMap((session) =>
    session.cells
      .filter((cell) => lane === undefined || cell.lane === undefined || cell.lane === lane)
      .map((cell) => ({ key: `${session.name}/${cell.name}`, session, cell }))
  )
  const cells = entries.filter((entry) => entry.session.aside === undefined)
  const lastSession = cells[cells.length - 1]?.session
  const latest = cells.filter((entry) => entry.session === lastSession)
  const ran = latest.filter((entry) => entry.cell.skipped === undefined)
  const active = entries.find((entry) => entry.key === cellKey) ??
    (queryCase === undefined ? undefined : latest.find((entry) => caseIdOf(entry.cell.name) === queryCase)) ??
    ran.find((entry) => !cellPassed(entry.cell)) ?? ran[0] ?? latest[0] ?? cells[0]
  const picker = active !== undefined && active.session.aside !== undefined ? [...cells, active] : cells
  return { cells, latest, active, picker }
}
