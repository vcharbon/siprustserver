/**
 * The lines a cell's lane still waits on: read as the cell lands, and read
 * again once its batch's end has answered them. A cell that only its pending
 * lines keep from passing lands not passed, naming them (`CellSummary.pending`);
 * {@link settled} gives its outcome once the end has written them.
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

/** The names of the lines deciding the cell in `cellDir` that its lane still waits on. */
export const pendingIn = Effect.fn("Driver.pendingIn")(function*(cellDir: string) {
  const path = yield* Path.Path
  return Verification.pending(yield* linesIn(path.join(cellDir, Verification.FILE))).map((line) => line.name)
})

/**
 * The summary of the cell in `cellDir` once its batch's end has answered:
 * unchanged where it named no pending line; else passed exactly where every
 * line it named is stated, none still pending and none failing, its
 * `pending` naming the ones still pending or no longer stated.
 */
export const settled = Effect.fn("Driver.settled")(function*(summary: E2e.CellSummary, cellDir: string) {
  if (summary.pending === undefined) return summary
  const path = yield* Path.Path
  const lines = yield* linesIn(path.join(cellDir, Verification.FILE))
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
