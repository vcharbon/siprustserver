/**
 * A case source (ADR-0060 appendix §16): a module `pack.toml`'s
 * `[cases] sources` names, whose default export is the compiled cells it
 * yields, an array of {@link CompiledCell.CompiledCell}. How a source builds
 * them is its own: the base imports the module and reads the array.
 *
 * A source is refused, naming its file, when it does not load, when its
 * default export is no array, when a cell does not decode or does not hold
 * (`compiled-cell.ts`), and when two of its cells state one id.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as path from "node:path"
import { pathToFileURL } from "node:url"
import * as CompiledCell from "./compiled-cell.js"
import { decoderOf, type Refusal, refuse } from "./manifest-file.js"
import * as Pack from "./pack.js"

/** One source a pack names: the path `pack.toml` states and the module's absolute path. */
export interface Named {
  readonly path: string
  readonly absolute: string
}

/** The sources the pack at `root` names, in its order; none where its manifest names none. */
export const namedBy = (root: string): Effect.Effect<ReadonlyArray<Named>, Refusal, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = path.join(root, Pack.FILE)
    const text = yield* fs.readFileString(file).pipe(Effect.mapError(() => refuse([root], `a pack holds ${Pack.FILE} at its root`)))
    const pack = yield* Effect.fromResult(Pack.parse(file, text))
    return (pack.cases?.sources ?? []).map((source) => ({ path: source, absolute: path.resolve(root, source) }))
  })

const decodeCell = decoderOf(CompiledCell.CompiledCell)

/** The cells `value`, the default export of the source module `file`, states, or why the source is refused. */
export const cellsOf = (file: string, value: unknown): Result.Result<ReadonlyArray<CompiledCell.CompiledCell>, Refusal> => {
  if (!Array.isArray(value)) return Result.fail(refuse([file], "its default export is no array of compiled cells"))
  const cells: Array<CompiledCell.CompiledCell> = []
  const ids = new Set<string>()
  for (const [index, raw] of value.entries()) {
    const decoded = decodeCell(raw)
    if (Result.isFailure(decoded)) return Result.fail(refuse([file], `cell ${index}: ${decoded.failure}`))
    const cell = decoded.success
    const why = CompiledCell.meaningOf(cell)
    if (why !== undefined) return Result.fail(refuse([file], `cell ${cell.id}: ${why}`))
    if (ids.has(cell.id)) return Result.fail(refuse([file], `two cells state the id ${cell.id}`))
    ids.add(cell.id)
    cells.push(cell)
  }
  return Result.succeed(cells)
}

/** Why a module did not load, on one line. */
const reasonOf = (error: unknown): string => (error instanceof Error ? error.message.split("\n")[0]! : String(error))

/** The cells the source module at `file` yields, or why it is refused. */
export const load = (file: string): Effect.Effect<ReadonlyArray<CompiledCell.CompiledCell>, Refusal> =>
  Effect.tryPromise({
    try: async () => ((await import(pathToFileURL(file).href)) as { readonly default?: unknown }).default,
    catch: (error) => refuse([file], `does not load: ${reasonOf(error)}`)
  }).pipe(Effect.flatMap((value) => Effect.fromResult(cellsOf(file, value))))
