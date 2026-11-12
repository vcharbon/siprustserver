/**
 * The lines a cell's lane answered as the cell lands, and those it still
 * waits on: read as the cell lands, and read again once its batch's end has
 * answered them. A cell that only its pending lines keep from passing lands
 * not passed, naming them (`CellSummary.pending`); {@link settled} gives its
 * outcome once the end has written them.
 */
import { E2e, Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as Path from "effect/Path"
import * as Schema from "effect/Schema"

const decodeLines = Schema.decodeUnknownOption(Schema.Array(Verification.Verification))

/** The lines of `file`; none where the file is absent or unreadable. */
const linesIn = Effect.fn("Driver.linesIn")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => "null"))
  const json = yield* Effect.try(() => JSON.parse(text) as unknown).pipe(Effect.orElseSucceed(() => null))
  return Option.getOrElse(decodeLines(json), () => [])
})

/** What the lane's lines of a cell say as it lands, by line name. */
export interface Landed {
  /** The lines deciding the cell that its lane still waits on. */
  readonly pending: ReadonlyArray<string>
  /** The lines failing the cell. */
  readonly failing: ReadonlyArray<string>
}

const namesOf = (lines: ReadonlyArray<Verification.Verification>): ReadonlyArray<string> => [
  ...new Set(lines.map((line) => line.name))
]

/** The lane's lines of the cell in `cellDir` as it lands. */
export const landedIn = Effect.fn("Driver.landedIn")(function*(cellDir: string) {
  const path = yield* Path.Path
  const lines = yield* linesIn(path.join(cellDir, Verification.FILE))
  return { pending: namesOf(Verification.pending(lines)), failing: namesOf(Verification.failing(lines)) } satisfies Landed
})

/**
 * `held` with `answered` merged in: each answered line in place of the held
 * `pending` one of its name and source, else after them.
 */
const mergedWith = (
  held: ReadonlyArray<Verification.Verification>,
  answered: ReadonlyArray<Verification.Verification>
): ReadonlyArray<Verification.Verification> => {
  const out = [...held]
  for (const line of answered) {
    const at = out.findIndex((one) => one.name === line.name && one.source === line.source && one.answer === "pending")
    if (at >= 0) out[at] = line
    else out.push(line)
  }
  return out
}

/**
 * The summary of the cell in `cellDir` once its batch's end has answered,
 * its lines read with `answered` (what its batch's collectors and judges
 * answered of it) merged in: unchanged where it named no pending line; else
 * passed exactly where every line it named is stated, none still pending and
 * none failing, its `pending` naming the ones still pending or no longer
 * stated.
 */
export const settled = Effect.fn("Driver.settled")(function*(
  summary: E2e.CellSummary,
  cellDir: string,
  answered: ReadonlyArray<Verification.Verification> = []
) {
  if (summary.pending === undefined) return summary
  const path = yield* Path.Path
  const lines = mergedWith(yield* linesIn(path.join(cellDir, Verification.FILE)), answered)
  const named = (name: string) => lines.filter((line) => line.name === name && Verification.decides(line))
  const still = summary.pending.filter((name) =>
    named(name).length === 0 || Verification.pending(named(name)).length > 0
  )
  const failed = summary.pending.some((name) => Verification.failing(named(name)).length > 0)
  const { pending: _landed, ...rest } = summary
  return {
    ...rest,
    passed: still.length === 0 && !failed,
    ...(still.length === 0 ? {} : { pending: still })
  } satisfies E2e.CellSummary
})
