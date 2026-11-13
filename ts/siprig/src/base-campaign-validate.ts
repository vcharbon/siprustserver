/**
 * The base's campaign validated with no lane stood, no store opened and no
 * call placed (`@sip/campaign-runner` `validate.ts`): the case sources a run
 * reads (`sources`, else the manifest's), compiled by the base run's own
 * compile phase (`base-campaign-cells.ts` `compile`), every document linted
 * by the `pivot-schema` the run lints with, the cells checked across, against
 * the workspace's declared tags and lanes (else the pack's). Each problem is
 * one line on stderr, `<level> <file> [<sub-case>]: <message>`; what only a
 * run settles is a `note:` line on stdout. With `out`, the compiled cells are
 * kept under `<out>/cells/`, with `cells.json` and `validation.json` beside
 * them, and with `expand`, each cell's `expanded.json`
 * (`@sip/campaign-runner` `expanded.ts`). Exit 0 with no problem, 1 with one,
 * 2 where the line or the configuration is refused.
 */
import { readExpanded } from "@sip/campaign-runner/expanded"
import { pivotLint } from "@sip/campaign-runner/lint"
import * as Validate from "@sip/campaign-runner/validate"
import { LaneCatalog } from "@sip/rig"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as path from "node:path"
import { lanesRootOf, manifestOf } from "./base-campaign.ts"
import { compile, selectionOf } from "./base-campaign-cells.ts"
import { readingOf } from "./base-lane-hosting.ts"
import { declaredOf } from "./base-workspace.ts"

/** What a base campaign validation is asked. */
export interface Options {
  /** The pack's directory, absolute. */
  readonly pack: string
  /** The workspace's root: its declared tags and its lanes; none where undefined. */
  readonly workspace: string | undefined
  /** Where the compiled cells and their descriptions are written; nothing kept where undefined. */
  readonly out: string | undefined
  /** Whether each cell's `expanded.json` is written beside its document (with `out` alone). */
  readonly expand: boolean
  /** The `pivot-schema` binary the compile and the validation lint with. */
  readonly pivotSchema: string
  /** The case source modules read, paths in the pack; every one its manifest names where undefined. */
  readonly sources?: ReadonlyArray<string> | undefined
}

/** The verb's name on its lines. */
const VERB = "siprig-campaign validate"

/** The validation `options` asks for: its exit code, a refusal said and answered 2. */
export const validate = (options: Options) =>
  Effect.gen(function*() {
    const refuse = (reason: string) => Effect.as(Console.error(`${VERB}: ${reason}`), 2 as const)
    if (options.expand && options.out === undefined) return yield* refuse("--expand writes beside the compiled documents: name --out")
    const manifest = yield* manifestOf(options.pack).pipe(Effect.result)
    if (Result.isFailure(manifest)) return yield* refuse(manifest.failure)
    const declared = yield* declaredOf(options.workspace).pipe(Effect.result)
    if (Result.isFailure(declared)) return yield* refuse(declared.failure)
    const lanesRoot = options.workspace ?? lanesRootOf(options.pack, manifest.success)
    const catalog = yield* LaneCatalog.load(readingOf(options.pack), LaneCatalog.dirsUnder(lanesRoot)).pipe(Effect.result)
    if (Result.isFailure(catalog)) return yield* refuse(catalog.failure.message)
    const selection = yield* selectionOf(options.pack, options.sources).pipe(Effect.result)
    if (Result.isFailure(selection)) return yield* refuse(selection.failure.message)
    const lint = yield* pivotLint(Effect.succeed(options.pivotSchema))
    const validation = yield* Validate.withCellsDir(options.out, (cellsDir) =>
      Validate.validate(compile(selection.success, cellsDir, options.pivotSchema, declared.success), {
        lint,
        lanes: catalog.success.lanes
      }))
    if (options.out !== undefined) {
      yield* Validate.writeOut(path.resolve(options.out), validation, options.expand ? (cell) => readExpanded(cell) : undefined)
    }
    const files = selection.success.selected.length + selection.success.broken.length
    yield* Console.log(`${VERB}: ${validation.cells.length} cell(s) from ${files} case source(s), ${validation.problems.length} problem(s)`)
    for (const note of validation.notes) yield* Console.log(`note: ${note}`)
    for (const line of Validate.problemLines(validation)) yield* Console.error(line)
    return Validate.exitOf(validation)
  })

export * as BaseCampaignValidate from "./base-campaign-validate.ts"
