/**
 * An SDK's `.d.ts` text, generated from the tooling's own sources
 * ({@link Sources}): the header, each exported type alias of the types file
 * expanded to plain literal and object types, each surface file as written
 * (its imports of the sources dropped), then one declared value per member of
 * the values interface. The text declares everything it names, so it
 * typechecks a workspace's files with no source of the tooling and no other
 * declaration on the path; {@link generate} refuses a text that does not
 * typecheck on its own.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import ts from "typescript"
import { declarationPathOf } from "./sdk-module.js"
import { hostWith, WORKSPACE_OPTIONS } from "./typecheck.js"

export class DeclarationsUngenerated extends Schema.TaggedError<DeclarationsUngenerated>()("CaseModules.DeclarationsUngenerated", {
  reason: Schema.String
}) {
  override get message(): string {
    return `the SDK's .d.ts: ${this.reason}`
  }
}

/** What an SDK's `.d.ts` is generated from. */
export interface Sources {
  /** The specifier the text declares, as a workspace file imports it. */
  readonly specifier: string
  /** The text's opening comment, as written. */
  readonly header: string
  /** The file (absolute) whose exported type aliases are expanded. */
  readonly types: string
  /** The files (absolute) kept as written, in order; each may import the sources alone. */
  readonly surfaces: ReadonlyArray<string>
  /** The interface, declared in a surface file, each member of which becomes a declared value. */
  readonly values: string
  /** The compiler options the sources are read under. */
  readonly options: ts.CompilerOptions
}

const NODE_FLAGS = ts.NodeBuilderFlags.NoTruncation | ts.NodeBuilderFlags.InTypeAlias |
  ts.NodeBuilderFlags.MultilineObjectLiterals | ts.NodeBuilderFlags.UseSingleQuotesForStringLiteralType

const leadingComment = (text: string, node: ts.Node): string =>
  (ts.getLeadingCommentRanges(text, node.getFullStart()) ?? []).map((range) => text.slice(range.pos, range.end)).join("\n")

const isExported = (node: ts.Node): boolean =>
  ts.canHaveModifiers(node) && (ts.getModifiers(node) ?? []).some((m) => m.kind === ts.SyntaxKind.ExportKeyword)

/** The specifier a source file at `file` is imported by from a sibling. */
const ownSpecifier = (file: string): string => `./${path.basename(file).replace(/\.ts$/, ".js")}`

/** Every exported alias of `file`, expanded. */
const expandedTypes = (program: ts.Program, file: ts.SourceFile): ReadonlyArray<string> => {
  const checker = program.getTypeChecker()
  const printer = ts.createPrinter({ removeComments: true })
  const out: Array<string> = []
  for (const statement of file.statements) {
    if (!ts.isTypeAliasDeclaration(statement) || !isExported(statement)) continue
    const type = checker.getTypeAtLocation(statement.name)
    const node = checker.typeToTypeNode(type, undefined, NODE_FLAGS)
    if (node === undefined) throw new Error(`${statement.name.text} has no expansion`)
    const printed = printer.printNode(ts.EmitHint.Unspecified, node, file)
    const comment = leadingComment(file.text, statement)
    out.push(`${comment === "" ? "" : `${comment}\n`}export type ${statement.name.text} = ${printed}\n`)
  }
  return out
}

/** A surface file without its imports of `own`; any other import is refused. */
const surfaceText = (file: ts.SourceFile, own: ReadonlySet<string>): string => {
  const kept: Array<string> = []
  for (const statement of file.statements) {
    if (ts.isImportDeclaration(statement)) {
      const from = (statement.moduleSpecifier as ts.StringLiteral).text
      if (!own.has(from)) throw new Error(`${path.basename(file.fileName)} imports ${from}: it may import the SDK's own sources alone`)
      continue
    }
    kept.push(file.text.slice(statement.getFullStart(), statement.getEnd()).replace(/^\n+/, ""))
  }
  return kept.join("\n\n")
}

/** One declared value per member of the interface `name` the files `surfaces` declare. */
const values = (program: ts.Program, surfaces: ReadonlyArray<ts.SourceFile>, name: string): ReadonlyArray<string> => {
  const declared = surfaces.flatMap((file) => file.statements)
    .find((s): s is ts.InterfaceDeclaration => ts.isInterfaceDeclaration(s) && s.name.text === name)
  if (declared === undefined) throw new Error(`no surface file declares an interface ${name}`)
  const checker = program.getTypeChecker()
  return checker.getPropertiesOfType(checker.getTypeAtLocation(declared.name)).map((member) =>
    `export declare const ${member.name}: ${name}["${member.name}"]`
  )
}

/** The diagnostics of `text` checked alone, as the `.d.ts` of `specifier`. */
export const diagnosticsOf = (specifier: string, text: string): ReadonlyArray<string> => {
  const program = ts.createProgram([declarationPathOf(specifier)], WORKSPACE_OPTIONS, hostWith(WORKSPACE_OPTIONS, { specifier, text }))
  return ts.getPreEmitDiagnostics(program).map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n"))
}

/** The text of the `.d.ts` `sources` state, or why it is refused. */
export const generate = (sources: Sources) =>
  Effect.try({
    try: () => {
      const program = ts.createProgram([sources.types, ...sources.surfaces], sources.options)
      const types = program.getSourceFile(sources.types)
      const surfaces = sources.surfaces.map((file) => program.getSourceFile(file))
      if (types === undefined || surfaces.some((file) => file === undefined)) throw new Error("the SDK sources are not found")
      const errors = ts.getPreEmitDiagnostics(program).map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n"))
      if (errors.length > 0) throw new Error(`the SDK sources do not typecheck: ${errors.join("; ")}`)
      const read = surfaces as ReadonlyArray<ts.SourceFile>
      const own = new Set([sources.types, ...sources.surfaces].map(ownSpecifier))
      return [
        sources.header,
        ...expandedTypes(program, types),
        ...read.flatMap((file) => [surfaceText(file, own), ""]),
        ...values(program, read, sources.values),
        ""
      ].join("\n")
    },
    catch: (e) => new DeclarationsUngenerated({ reason: e instanceof Error ? e.message : String(e) })
  }).pipe(
    Effect.flatMap((text) => {
      const errors = diagnosticsOf(sources.specifier, text)
      return errors.length === 0
        ? Effect.succeed(text)
        : Effect.fail(new DeclarationsUngenerated({ reason: `it does not typecheck on its own: ${errors.slice(0, 5).join("; ")}` }))
    })
  )
