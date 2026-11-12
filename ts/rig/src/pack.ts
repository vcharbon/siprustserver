/**
 * The pack manifest, `pack.toml` at a pack's root (`pack.v1`, ADR-0060
 * appendix §16): one product's name and wrapper, what its image adds, its
 * provisioning, its case sources, its deployment module and default lanes,
 * the component name its code is recorded under, and the paths of its tree
 * that decide what an extraction and a cut produce.
 */
import * as Schema from "effect/Schema"
import { Name, parseWith, RelativePath, Text } from "./manifest-file.js"

/** The schema version a pack manifest states. */
export const SCHEMA = "pack.v1"

/** The manifest's file name at the pack's root. */
export const FILE = "pack.toml"

const Image = Schema.Struct({
  /** OS packages the pack's image adds to the base image. */
  packages: Schema.optionalKey(Schema.Array(Text)),
  /** The directory of the product binaries the image carries, the replay one included when it registers. */
  binaries: Schema.optionalKey(RelativePath)
})

const Provision = Schema.Struct({
  /** The command run once per job before any batch; empty, no provisioning. */
  run: Schema.String
})

const Cases = Schema.Struct({
  /** Modules whose default export is the compiled cells they yield (`case-source.ts`). */
  sources: Schema.Array(RelativePath)
})

const Modules = Schema.Struct({
  /** The module filling the pipeline and corpus seams. */
  deployment: Schema.optionalKey(RelativePath),
  /** The directory of the default lane and part files. */
  lanes: Schema.optionalKey(RelativePath)
})

/** The paths of the pack's tree whose content decides a step's output: a change under one re-runs the step. */
const Decides = Schema.Struct({
  extraction: Schema.optionalKey(Schema.Array(RelativePath)),
  cut: Schema.optionalKey(Schema.Array(RelativePath))
})

export const Pack = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  name: Name,
  /** What every page shows beside the rig's name. */
  title: Text,
  /** The command a user types. */
  wrapper: Name,
  /** The component a run records the pack's code under; default its `name`. */
  component: Schema.optionalKey(Name),
  image: Schema.optionalKey(Image),
  provision: Schema.optionalKey(Provision),
  cases: Schema.optionalKey(Cases),
  modules: Schema.optionalKey(Modules),
  decides: Schema.optionalKey(Decides),
  /** The sources the pack's own code answers verification lines as: no collector is named like one. */
  sources: Schema.optionalKey(Schema.Array(Name))
})
export interface Pack extends Schema.Schema.Type<typeof Pack> {}

/** The component a run records the pack's code under. */
export const componentOf = (pack: Pack): string => pack.component ?? pack.name

/** A path the manifest names that exists in the pack's tree: its dotted key, the path, and whether it is a directory. */
export interface NamedPath {
  readonly key: string
  readonly path: string
  readonly is: "file" | "directory" | "either"
}

/** Every path the manifest names that the pack's tree holds; the image's binaries are the image's. */
export const pathsOf = (pack: Pack): ReadonlyArray<NamedPath> => [
  ...(pack.cases?.sources ?? []).map((path): NamedPath => ({ key: "cases.sources", path, is: "file" })),
  ...(pack.modules?.deployment === undefined ? [] : [{ key: "modules.deployment", path: pack.modules.deployment, is: "file" } as const]),
  ...(pack.modules?.lanes === undefined ? [] : [{ key: "modules.lanes", path: pack.modules.lanes, is: "directory" } as const]),
  ...(pack.decides?.extraction ?? []).map((path): NamedPath => ({ key: "decides.extraction", path, is: "either" })),
  ...(pack.decides?.cut ?? []).map((path): NamedPath => ({ key: "decides.cut", path, is: "either" }))
]

/** The pack manifest `file` holds as `text`, or why it is none. */
export const parse = parseWith(Pack, () => undefined)
