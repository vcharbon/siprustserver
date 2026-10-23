/**
 * What a landed cell keeps of its terminated calls: the interpreter writes
 * `call.terminated.json` for every cell, and once the cell's final verdict is
 * in, a failing cell keeps it gzip-compressed as `call.terminated.json.gz` and
 * a passing one keeps none — the call it holds is what a differential of a
 * failure reads, and a green cell's is remade by a replay.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import * as zlib from "node:zlib"

/** The file the interpreter writes beside `verdict.json`. */
export const TERMINATED = "call.terminated.json"

/** The cell directory's terminated calls, kept compressed where the cell failed, dropped where it passed. */
export const trimTerminated = (cellDir: string, passed: boolean): Effect.Effect<void, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = path.join(cellDir, TERMINATED)
    if (!(yield* fs.exists(file))) return
    if (!passed) yield* fs.writeFile(`${file}.gz`, zlib.gzipSync(yield* fs.readFile(file)))
    yield* fs.remove(file)
  }).pipe(Effect.orDie)
