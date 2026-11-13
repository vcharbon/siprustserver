/**
 * The cells of a base campaign: every case source the pack's `pack.toml`
 * names (`[cases] sources`, `@sip/rig` `CaseSource`), each a case file named
 * by its path in the pack, loaded or broken, compiled by the runner's compile
 * phase (`@sip/campaign-runner` `cases.ts`): each cell's claims read as the
 * simulated lane answers them, its document passing the pivot schema lint of
 * the binary {@link pivotSchemaOf} names, written under the run's `cells/`.
 * Every cell runs the post-run cleanup and RFC audit gates.
 */
import { type CaseCompiler, compileCases } from "@sip/campaign-runner/cases"
import type { Broken, CaseFile, Selected } from "@sip/campaign-runner/case-file"
import type { Cell } from "@sip/campaign-runner/cell"
import { CLEANUP_GATE, RFC_GATE } from "@sip/campaign-runner/checked"
import { pivotLint } from "@sip/campaign-runner/lint"
import { cellOfSource } from "@sip/campaign-runner/source-cells"
import { Postcondition } from "@sip/contracts"
import { CaseSource, CompiledCell } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** The variable naming the `pivot-schema` binary the lint runs, ahead of any other place. */
export const PIVOT_SCHEMA_VARIABLE = "PIVOT_SCHEMA_BIN"

/**
 * The `pivot-schema` binary `env` names: {@link PIVOT_SCHEMA_VARIABLE}, else
 * the one in the `SIPRIG_BIN_DIR` directory, else the release build in
 * `checkout`.
 */
export const pivotSchemaOf = (env: NodeJS.ProcessEnv, checkout: string): string => {
  const named = env[PIVOT_SCHEMA_VARIABLE]
  if (named !== undefined && named !== "") return path.resolve(named)
  const dir = env["SIPRIG_BIN_DIR"]
  if (dir !== undefined && dir !== "") return path.join(path.resolve(dir), "pivot-schema")
  return path.join(checkout, "target", "release", "pivot-schema")
}

/** Whether `inner` is `root` or a path under it, segment by segment. */
const within = (root: string, inner: string): boolean => inner === root || inner.startsWith(`${root}${path.sep}`)

/**
 * Whether the relative `source` stays in the pack at `pack` once every link
 * is followed; a path leading nowhere is judged as written.
 */
const staysIn = (pack: string, source: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const named = path.resolve(pack, source)
    const real = yield* fs.realPath(named).pipe(Effect.option)
    if (real._tag === "None") return within(path.resolve(pack), named)
    return within(yield* fs.realPath(pack).pipe(Effect.orElseSucceed(() => path.resolve(pack))), real.value)
  })

/**
 * The case sources a run reads: `sources` where the run names them, else
 * every one the pack's manifest names, in its order. A named relative path is
 * one in the pack, and is refused where it leads out of the pack's tree, its
 * links followed; an absolute one, a cut's output say, is taken as named.
 */
const namedOf = (pack: string, sources: ReadonlyArray<string> | undefined) =>
  Effect.gen(function*() {
    if (sources === undefined) return yield* CaseSource.namedBy(pack)
    for (const source of sources) {
      if (!path.isAbsolute(source) && !(yield* staysIn(pack, source))) {
        return yield* Effect.fail({ message: `the case source ${source} leads out of the pack ${pack}` })
      }
    }
    return sources.map((source) => ({ path: source, absolute: path.resolve(pack, source) }))
  })

/**
 * What a cut case claims as a cell of the base's campaign: the one call
 * record the system writes of each call its document places, which the
 * simulated lane counts of every cell.
 */
const recordPerCall = (document: CompiledCell.CompiledCell["document"]): CompiledCell.CompiledCell["claims"] => ({
  [Postcondition.CALL_RECORDS]: { count: document.calls.length }
})

/**
 * Every case source the run reads of the pack at `pack` (`sources`, else the
 * manifest's), in its order, each loaded or broken; a manifest that does not
 * read, or a named source out of the pack's tree, refuses.
 */
export const selectionOf = (pack: string, sources?: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const named = yield* namedOf(pack, sources)
    const fs = yield* FileSystem.FileSystem
    const selected: Array<CaseFile> = []
    const broken: Array<Broken> = []
    for (const source of named) {
      const file = path.normalize(source.path)
      const isCut = yield* fs.stat(source.absolute).pipe(
        Effect.map((info) => info.type === "Directory"),
        Effect.orElseSucceed(() => false)
      )
      const cells = yield* (isCut ? CaseSource.ofCut(source.absolute, recordPerCall) : CaseSource.load(source.absolute)).pipe(
        Effect.result
      )
      if (cells._tag === "Success") selected.push({ file, absolute: source.absolute, value: cells.success, cells: cells.success })
      else broken.push({ file, absolute: source.absolute, reason: cells.failure.reason })
    }
    return {
      selected,
      broken,
      partial: false,
      inCampaign: named.length,
      files: named.map((source) => ({ file: path.normalize(source.path), absolute: source.absolute }))
    } satisfies Selected
  })

const SOURCES_ONLY = { message: "the base compiles case sources alone" }

/** The base's case compiler: a case source's cell as the runner places it, nothing beside it. */
export const COMPILER: CaseCompiler<Cell, never, Selected, FileSystem.FileSystem> = {
  compile: () => Effect.fail(SOURCES_ONLY),
  linted: () => false,
  write: () => Effect.fail(SOURCES_ONLY),
  statedTags: () => [],
  gates: [CLEANUP_GATE, RFC_GATE],
  claimsOf: () => ({}),
  cellOf: (_compiled, cell) => cell,
  writeSource: (dir, compiled) => CompiledCell.write(dir, compiled).pipe(Effect.mapError((e) => ({ message: String(e) }))),
  cellOfSource
}

/** The compile phase over `selection` into `cellsDir`, its documents linted by the `pivot-schema` at `binary`. */
export const compile = (selection: Selected, cellsDir: string, binary: string) =>
  Effect.flatMap(
    pivotLint(Effect.succeed(binary)),
    (lint) => compileCases(COMPILER, selection, cellsDir, null, { lint, declared: { file: "", tags: {}, tagCases: [] } })
  )

export * as BaseCampaignCells from "./base-campaign-cells.ts"
