/**
 * A compiled cell (ADR-0060 appendix §16): what a case source yields for one
 * cell. Its id names its directory, the case and sub-case it plays name its
 * results, the pivot document is what the interpreter runs, the resources are
 * the files the document's bodies name, and the claims (§4) ride beside the
 * document, never in it.
 *
 * A cell is refused when a resource path leaves its directory, or when a body
 * of its document names a resource the cell does not carry. A cell asking for
 * an operation its lane does not bind is not runnable there (`Lanes.unboundOf`).
 */
import { Body, Pivot } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { Name, RelativePath, Text } from "./manifest-file.js"

/** The file a cell's claims are written to, beside its document. */
export const CLAIMS_FILE = "claims.json"

/** The file a cell's document is written to, in its directory. */
export const DOCUMENT_FILE = "scenario.json"

export const CompiledCell = Schema.Struct({
  /** The cell's directory name, unique among the cells of a run. */
  id: Name,
  /** The case it plays: what its results are filed under. */
  case: Text,
  /** The sub-case of the case it plays, where the case has several. */
  sub_case: Schema.optionalKey(Text),
  /** What a reader is shown first. */
  title: Text,
  document: Pivot.PivotV3,
  /** Each resource file by its path relative to the cell's directory, as the document's bodies name it. */
  resources: Schema.Record(RelativePath, Schema.String),
  /** What the case expects of each collector, by its name (§4), opaque to the base. */
  claims: Schema.Record(Name, Schema.Unknown),
  /** The operation names its steps ask for (§7): a lane binding none of one cannot run the cell. */
  operations: Schema.optionalKey(Schema.Array(Name))
})
export interface CompiledCell extends Schema.Schema.Type<typeof CompiledCell> {}

/** Every resource the document's bodies name, multipart parts included. */
const resourcesNamed = (document: Pivot.PivotV3): ReadonlyArray<string> =>
  Pivot.pivotSteps(document).flatMap((step) => {
    const body = step.msg.body
    if (body === undefined) return []
    if (Body.isResourceBody(body)) return [body.ref]
    if (Body.isMultipartBody(body)) return body.multipart.parts.map((part) => part.ref)
    return []
  })

/** Why `cell` does not hold, or `undefined`: a body naming a resource the cell does not carry. */
export const meaningOf = (cell: CompiledCell): string | undefined => {
  const missing = resourcesNamed(cell.document).find((ref) => !(ref in cell.resources))
  return missing === undefined ? undefined : `its document names the resource ${missing}, which the cell does not carry`
}

/**
 * Write `cell` under `dir`: `<dir>/<id>/` holding its document in the
 * canonical form, its resources and its claims. The cell's directory.
 */
export const write = Effect.fn("Rig.CompiledCell.write")(function*(dir: string, cell: CompiledCell) {
  const fs = yield* FileSystem.FileSystem
  const cellDir = path.join(dir, cell.id)
  yield* fs.makeDirectory(cellDir, { recursive: true })
  yield* fs.writeFileString(path.join(cellDir, DOCUMENT_FILE), Pivot.emitPivot(cell.document))
  yield* fs.writeFileString(path.join(cellDir, CLAIMS_FILE), `${JSON.stringify(cell.claims, null, 2)}\n`)
  for (const [resource, content] of Object.entries(cell.resources)) {
    const file = path.join(cellDir, resource)
    yield* fs.makeDirectory(path.dirname(file), { recursive: true })
    yield* fs.writeFileString(file, content)
  }
  return cellDir
})
