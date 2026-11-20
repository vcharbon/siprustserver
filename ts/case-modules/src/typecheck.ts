/**
 * A workspace's files typechecked against the SDK's declaration text alone,
 * under {@link WORKSPACE_OPTIONS} (the options the workspace's own
 * `tsconfig.json` states): the SDK's specifier maps to that text, nothing of
 * the tooling is on the path. Answers the errors of each file,
 * `file:line:column: message`; a file absent from the answer has none. An
 * error in a module the files import (a shared module, the SDK) is the error
 * of every file whose imports reach that module.
 */
import * as Effect from "effect/Effect"
import * as path from "node:path"
import ts from "typescript"
import { declarationPathOf, type SdkText } from "./sdk-module.js"

/** How a workspace's files are checked: the SDK's specifier mapped by {@link typecheck}. */
export const WORKSPACE_OPTIONS: ts.CompilerOptions = {
  target: ts.ScriptTarget.ES2022,
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  lib: ["lib.es2023.d.ts"],
  types: [],
  strict: true,
  noEmit: true,
  skipLibCheck: false
}

/** A compiler host reading `sdk`'s text at its declaration path, every other file from disk. */
export const hostWith = (options: ts.CompilerOptions, sdk: SdkText): ts.CompilerHost => {
  const host = ts.createCompilerHost(options)
  const at = declarationPathOf(sdk.specifier)
  return {
    ...host,
    fileExists: (name) => name === at || host.fileExists(name),
    readFile: (name) => (name === at ? sdk.text : host.readFile(name)),
    getSourceFile: (name, version, onError) =>
      name === at ? ts.createSourceFile(name, sdk.text, version, true) : host.getSourceFile(name, version, onError)
  }
}

const lineOf = (d: ts.Diagnostic, shown: (file: string) => string): string => {
  const message = ts.flattenDiagnosticMessageText(d.messageText, " ")
  if (d.file === undefined || d.start === undefined) return message
  const { line, character } = d.file.getLineAndCharacterOfPosition(d.start)
  return `${shown(d.file.fileName)}:${line + 1}:${character + 1}: ${message}`
}

/** Each module of `program` (absolute), with the modules its imports resolve to. */
const importsOf = (program: ts.Program, options: ts.CompilerOptions, host: ts.CompilerHost) => {
  const graph = new Map<string, ReadonlyArray<string>>()
  for (const file of program.getSourceFiles()) {
    const resolved = ts.preProcessFile(file.text, true, true).importedFiles.flatMap((imported) => {
      const to = ts.resolveModuleName(imported.fileName, file.fileName, options, host).resolvedModule?.resolvedFileName
      return to === undefined ? [] : [path.resolve(to)]
    })
    graph.set(path.resolve(file.fileName), resolved)
  }
  return graph
}

/** The modules `root` reaches through its imports, itself included. */
const closureOf = (root: string, graph: ReadonlyMap<string, ReadonlyArray<string>>): ReadonlySet<string> => {
  const seen = new Set<string>()
  const walk = (at: string): void => {
    if (seen.has(at)) return
    seen.add(at)
    for (const next of graph.get(at) ?? []) walk(next)
  }
  walk(root)
  return seen
}

/**
 * The errors of each of `files` (absolute) checked against the SDK `sdk`,
 * each file named by `shown`; an error in an imported module is each
 * importing file's, an error of no file every file's.
 */
export const typecheck = (
  files: ReadonlyArray<string>,
  sdk: SdkText,
  shown: (file: string) => string = (file) => file
) =>
  Effect.sync(() => {
    const options: ts.CompilerOptions = { ...WORKSPACE_OPTIONS, paths: { [sdk.specifier]: [declarationPathOf(sdk.specifier)] } }
    const host = hostWith(options, sdk)
    const program = ts.createProgram([...files], options, host)
    const errors = new Map<string, Array<string>>()
    const add = (file: string, line: string) => errors.set(file, [...(errors.get(file) ?? []), line])
    const mine = new Set(files.map((file) => path.resolve(file)))
    const graph = importsOf(program, options, host)
    const reach = new Map([...mine].map((root) => [root, closureOf(root, graph)] as const))
    for (const d of ts.getPreEmitDiagnostics(program)) {
      if (d.category !== ts.DiagnosticCategory.Error) continue
      const at = d.file === undefined ? undefined : path.resolve(d.file.fileName)
      const line = lineOf(d, shown)
      if (at !== undefined && mine.has(at)) add(at, line)
      else for (const root of mine) if (at === undefined || reach.get(root)!.has(at)) add(root, line)
    }
    return errors as ReadonlyMap<string, ReadonlyArray<string>>
  })
