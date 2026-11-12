/**
 * The compile phase over a pack's case compiler ({@link CaseCompiler}): every
 * case file a run selects compiled into one cell per sub-case and written
 * under the run's `cells/`, each cell keeping the case file's text as
 * compiled. A file the compiler refuses is BROKEN, like one that did not
 * load: named with why, the others still compile. Two sub-cases compiling to
 * one cell id fail the phase, naming both, whatever the run keeps: the id
 * names the cell's directory, its run-spec, its bundle and the simulated
 * lane's Call-IDs.
 *
 * A document a workspace module plays passes the pivot schema lint
 * (`lint.ts`) or its file is BROKEN, naming each finding; a lint that does
 * not run (no binary, a crash on the document) breaks that file alone, naming
 * why. The cells of a case source (`source-cells.ts`) have their claims read
 * as the simulated lane answers them, and their documents so stated pass the
 * lint, or the source is broken. A claim its document states that nothing on
 * a lane answers is not requested, or refuses the batch where its collector is
 * required (`claims.ts`).
 */
import type { Pivot } from "@sip/contracts"
import { CompiledCell } from "@sip/rig"
import type { Declared } from "@sip/workspace/campaign-tags"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import { readFile } from "node:fs/promises"
import * as path from "node:path"
import { type Broken, type CaseFile, refOf, type Selected } from "./case-file.js"
import type { Cell, Checked, Heal } from "./cell.js"
import { checkedOf } from "./checked.js"
import { unansweredOf } from "./claims.js"
import type { Lint } from "./lint.js"
import * as Results from "@sip/store/campaign-results"
import { type CompiledCells, PhaseFailed } from "./run.js"
import type { Only } from "./scope.js"
import { type Answered, answeredOf, type Placing } from "./source-cells.js"
import { isAlone, tagsOfFile } from "./selection.js"

const failed = (reason: string) => new PhaseFailed({ phase: "compile", reason })

/** A sub-case as a pack's compiler compiles it: what the runner's cell takes of it. */
export interface Compiled {
  readonly id: string
  readonly ref: string
  readonly subcase: string
  readonly retries: number
  readonly heal: Heal
  readonly locks: ReadonlyArray<string>
  readonly needs: ReadonlyArray<string>
  readonly document: Pivot.PivotV3
  readonly laneTimers?: Readonly<Partial<Record<string, number>>>
}

/** How a pack compiles its case files `S` into its sub-cases `K` and its cells `C`. */
export interface CaseCompiler<C extends Cell, K extends Compiled, S extends Selected, R> {
  /** The sub-cases `file` of `selection` compiles to, or why the compiler refuses it. */
  readonly compile: (file: CaseFile, selection: S) => Effect.Effect<ReadonlyArray<K>, { readonly message: string }, R>
  /** Whether a workspace module plays `compiled`'s document: it passes the lint. */
  readonly linted: (compiled: K) => boolean
  /** `compiled` written under `dir`: its cell's directory. */
  readonly write: (dir: string, compiled: K) => Effect.Effect<string, { readonly message: string }, R>
  /** The tags a case states of itself, off its compiled sub-cases. */
  readonly statedTags: (compiled: ReadonlyArray<K>) => ReadonlyArray<string>
  /** The gates every cell runs after its steps' checks, in order (`checked.ts`). */
  readonly gates: ReadonlyArray<Checked>
  /** The claims `compiled` states beside its document. */
  readonly claimsOf: (compiled: K) => Readonly<Record<string, unknown>>
  /** The pack's cell of `compiled`, its runner fields `cell`. */
  readonly cellOf: (compiled: K, cell: Cell) => C
  /** A case source's cell written under `dir` with what the pack's judge reads: its cell's directory. */
  readonly writeSource: (dir: string, compiled: CompiledCell.CompiledCell) => Effect.Effect<string, { readonly message: string }, R>
  /** The pack's cell of a case source's cell. */
  readonly cellOfSource: (answered: Answered, placing: Placing) => C
}

/** What a run's compile reads beside the selection: the lint a workspace module's documents pass, the tags the deployment declares. */
export interface CompileContext {
  readonly lint: Lint
  readonly declared: Declared
}

/** The endpoint the first call's routed b-legs land on: its first attempt's leg's, else its caller's. */
export const egressOf = (document: Pivot.PivotV3): string | undefined => {
  const call = document.calls[0]
  if (call === undefined) return undefined
  const legId = call.attempts[0]?.leg ?? call.caller_leg
  const leg = document.legs.find((l) => l.id === legId)
  return document.actors.find((a) => a.id === leg?.actor)?.endpoint
}

const read = (file: CaseFile) =>
  Effect.tryPromise({
    try: () => readFile(file.absolute, "utf8"),
    catch: (e) => failed(`${file.file}: does not read: ${e instanceof Error ? e.message : String(e)}`)
  })

/** The runner's cell of `compiled` under `dir`, or the phase's failure where its document routes its b-legs to no endpoint. */
const cellOf = <C extends Cell, K extends Compiled>(
  compiler: Pick<CaseCompiler<C, K, never, never>, "gates" | "claimsOf" | "cellOf">,
  compiled: K,
  dir: string,
  source: Cell["source"],
  tags: ReadonlyArray<string>,
  alone: boolean
): Effect.Effect<C, PhaseFailed> => {
  const egress = egressOf(compiled.document)
  if (egress === undefined) return Effect.fail(failed(`${compiled.id}: no endpoint its b-legs land on`))
  const notRequested = unansweredOf(compiled.document, compiler.claimsOf(compiled))
  return Effect.succeed(compiler.cellOf(compiled, {
    id: compiled.id,
    ref: compiled.ref,
    subcase: compiled.subcase,
    retries: compiled.retries,
    tags,
    alone,
    heal: compiled.heal,
    source,
    checked: checkedOf(compiled.document, compiler.gates),
    locks: compiled.locks,
    needs: compiled.needs,
    dir,
    endpoints: compiled.document.endpoints.map((e) => e.id),
    egress,
    ...(compiled.laneTimers === undefined ? {} : { laneTimers: compiled.laneTimers }),
    ...(notRequested.length === 0 ? {} : { notRequested })
  }))
}

/** Whether the cell of `ref` and `subcase` is one `only` keeps. */
const kept = (only: Only | null, { ref, subcase }: { readonly ref: string; readonly subcase: string }): boolean =>
  only === null || (ref === only.ref && (only.subcase === null || subcase === only.subcase))

/**
 * The lint findings of the document `write` writes into a scratch directory
 * (its case directory returned), each named by `id`; the scratch directory is
 * removed after.
 */
const linted = <E extends { readonly message: string }, R>(
  id: string,
  write: (scratch: string) => Effect.Effect<string, E, R>,
  lint: Lint
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const scratch = yield* fs.makeTempDirectory({ prefix: "case-lint-" }).pipe(Effect.mapError((e) => failed(e.message)))
    const found = yield* write(scratch).pipe(
      Effect.mapError((e) => failed(`write ${id}: ${e.message}`)),
      Effect.flatMap((dir) => lint(path.join(dir, "scenario.json")).pipe(Effect.mapError((e) => failed(e.message)))),
      Effect.ensuring(fs.remove(scratch, { recursive: true }).pipe(Effect.ignore))
    )
    return found.map((f) => `${id}: ${f}`)
  })

/** The lint findings of each document of `compiled` a workspace module plays, each named by its cell id. */
const lintFindings = <K extends Compiled, R>(
  compiler: Pick<CaseCompiler<never, K, never, R>, "linted" | "write">,
  compiled: ReadonlyArray<K>,
  lint: Lint
) =>
  Effect.gen(function*() {
    const findings: Array<string> = []
    for (const one of compiled) {
      if (!compiler.linted(one)) continue
      findings.push(...yield* linted(one.id, (scratch) => compiler.write(scratch, one), lint))
    }
    return findings
  })

/** Why a case source's cells do not run: its file broken, or the phase failed. */
type SourceRefusal =
  | { readonly _tag: "Broken"; readonly broken: Broken }
  | { readonly _tag: "Failed"; readonly failed: PhaseFailed }

/**
 * The cells of the case source `file`, the ones `only` keeps, written under
 * `cellsDir` and pushed onto `into`, each id taken in `ids`; every cell's
 * claims read as the simulated lane answers them and its document, so stated,
 * passing the pivot schema lint, or the source is broken naming each refusal
 * and finding. A cell id another sub-case compiled to fails the phase.
 */
const sourceCells = <C extends Cell, R>(
  compiler: Pick<CaseCompiler<C, never, never, R>, "gates" | "writeSource" | "cellOfSource">,
  file: CaseFile,
  yielded: ReadonlyArray<CompiledCell.CompiledCell>,
  cellsDir: string,
  only: Only | null,
  context: CompileContext,
  ids: Map<string, string>,
  into: Array<C>
) =>
  Effect.gen(function*() {
    const brokenFor = (reason: string): SourceRefusal => ({ _tag: "Broken", broken: { file: file.file, absolute: file.absolute, reason } })
    const reading = yielded.map(answeredOf)
    const refused = reading.flatMap((r) => (Result.isFailure(r) ? [r.failure] : []))
    if (refused.length > 0) return brokenFor(refused.join("; "))
    const answered = reading.flatMap((r) => (Result.isSuccess(r) ? [r.success] : []))
    const findings: Array<string> = []
    for (const { compiled: cell } of answered) {
      const found = yield* linted(cell.id, (dir) => CompiledCell.write(dir, cell), context.lint).pipe(Effect.result)
      if (Result.isFailure(found)) return brokenFor(`the pivot schema lint did not run: ${found.failure.reason}`)
      findings.push(...found.success)
    }
    if (findings.length > 0) return brokenFor(`fails the pivot schema lint: ${findings.join("; ")}`)
    const source = { file: file.file, text: yield* read(file) }
    const tags = tagsOfFile(context.declared, file, [])
    for (const { compiled: cell } of answered) {
      const named = `${Results.caseIdOf(cell.case, cell.sub_case)} (${source.file})`
      const taken = ids.get(cell.id)
      if (taken !== undefined) {
        return { _tag: "Failed", failed: failed(`two sub-cases compile to the cell ${cell.id}: ${taken} and ${named}`) } satisfies SourceRefusal
      }
      ids.set(cell.id, named)
    }
    for (const one of answered) {
      const cell = one.compiled
      if (!kept(only, { ref: cell.case, subcase: cell.sub_case ?? "" })) continue
      const egress = egressOf(cell.document)
      if (egress === undefined) return { _tag: "Failed", failed: failed(`${cell.id}: no endpoint its b-legs land on`) } satisfies SourceRefusal
      const dir = yield* compiler.writeSource(cellsDir, cell).pipe(Effect.mapError((e) => failed(`write ${cell.id}: ${e.message}`)))
      into.push(compiler.cellOfSource(one, { dir, source, tags, alone: isAlone(context.declared, tags), egress, gates: compiler.gates }))
    }
    return undefined
  })

/**
 * Compile every file `selection` holds into cells under `cellsDir`, the ones
 * `only` keeps where it names some; the files broken are the selection's own,
 * then those the compiler refused.
 */
export const compileCases = <C extends Cell, K extends Compiled, S extends Selected, R>(
  compiler: CaseCompiler<C, K, S, R>,
  selection: S,
  cellsDir: string,
  only: Only | null,
  context: CompileContext
) =>
  Effect.gen(function*() {
    const cells: Array<C> = []
    const broken: Array<Broken> = [...selection.broken]
    const ids = new Map<string, string>()
    for (const file of selection.selected) {
      if (file.cells !== undefined) {
        const refused = yield* sourceCells(compiler, file, file.cells, cellsDir, only, context, ids, cells)
        if (refused?._tag === "Failed") return yield* Effect.fail(refused.failed)
        if (refused?._tag === "Broken") broken.push(refused.broken)
        continue
      }
      const compiled = yield* compiler.compile(file, selection).pipe(Effect.result)
      if (Result.isFailure(compiled)) {
        broken.push({ file: file.file, absolute: file.absolute, reason: compiled.failure.message })
        continue
      }
      const unlinted = yield* lintFindings(compiler, compiled.success, context.lint).pipe(Effect.result)
      if (Result.isFailure(unlinted)) {
        broken.push({ file: file.file, absolute: file.absolute, reason: `the pivot schema lint did not run: ${unlinted.failure.reason}` })
        continue
      }
      if (unlinted.success.length > 0) {
        broken.push({ file: file.file, absolute: file.absolute, reason: `fails the pivot schema lint: ${unlinted.success.join("; ")}` })
        continue
      }
      const source = { file: file.file, text: yield* read(file) }
      const tags = tagsOfFile(context.declared, file, compiler.statedTags(compiled.success))
      const alone = isAlone(context.declared, tags)
      for (const one of compiled.success) {
        const named = `${Results.caseIdOf(one.ref, one.subcase)} (${source.file})`
        const taken = ids.get(one.id)
        if (taken !== undefined) {
          return yield* Effect.fail(failed(`two sub-cases compile to the cell ${one.id}: ${taken} and ${named}`))
        }
        ids.set(one.id, named)
      }
      for (const one of compiled.success) {
        if (!kept(only, one)) continue
        const dir = yield* compiler.write(cellsDir, one).pipe(Effect.mapError((e) => failed(`write ${one.id}: ${e.message}`)))
        cells.push(yield* cellOf(compiler, one, dir, source, tags, alone))
      }
    }
    if (only !== null && cells.length === 0) {
      const what = only.subcase === null ? only.ref : `${only.ref} [${only.subcase}]`
      return yield* Effect.fail(failed(`no selected case file compiles ${what}`))
    }
    return { cells: cells as ReadonlyArray<C>, broken: broken as ReadonlyArray<Broken> } satisfies CompiledCells<C>
  })

/** The ref a case file's text states (its first `ref: "…"`), where one reads off it. */
export const refOfText = (text: string): string | undefined => /\bref:\s*(["'`])((?:(?!\1)[^\\\n])*)\1/.exec(text)?.[2]

/**
 * The ref of every case file `selection` holds: what a run of every case has
 * ported. A broken file counts by the ref its text states, so a file that no
 * longer loads is not reported unported.
 */
export const portedRefs = (selection: Selected): Effect.Effect<ReadonlyArray<string>> =>
  Effect.gen(function*() {
    const refs = selection.selected.flatMap((file) => {
      const ref = refOf(file.value)
      return ref === undefined ? [] : [ref]
    })
    for (const file of selection.broken) {
      const text = yield* Effect.promise(() => readFile(file.absolute, "utf8").catch(() => ""))
      const ref = refOfText(text)
      if (ref !== undefined) refs.push(ref)
    }
    return refs
  })
