/**
 * What a phase's INPUTS came to, as one hash: the source paths the
 * deployment's `Tools.decides` names for the phase, and for the cut the census
 * run it reads.
 *
 * Content, not commits: a working tree runs mid-edit, so a sha cannot say
 * whether an extraction or a cut is still current, and the whole-tree
 * fingerprint a run records (`provenance.ts`) moves on every unrelated edit —
 * under it nothing is ever current. Each fingerprint here names exactly what
 * decides its phase's output, so an edit anywhere else leaves standing work
 * standing.
 *
 * A path the tree does not hold contributes nothing. It cannot: a phase whose
 * code is missing cannot run at all, and refusing here would turn a missing
 * checkout into an unreadable hash error. Each fingerprint is read once per
 * process: a campaign's thousands of captures share one tree.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as crypto from "node:crypto"
import * as path from "node:path"
import * as Census from "./cut/census.js"
import { Roots, Tools } from "./deployment/index.js"

/** One hashed input: the name it rides under, and the file holding its bytes. */
export interface Input {
  readonly name: string
  readonly file: string
}

/**
 * Every file under a root-relative path, named relative to the root; a file
 * path is itself. A link met inside a directory is no file of it.
 */
const filesUnder = (
  fs: FileSystem.FileSystem,
  root: string,
  relative: string,
  inside = false
): Effect.Effect<ReadonlyArray<Input>, never> =>
  Effect.gen(function*() {
    const absolute = path.join(root, relative)
    if (inside && (yield* fs.readLink(absolute).pipe(Effect.as(true), Effect.orElseSucceed(() => false)))) return []
    const info = yield* fs.stat(absolute).pipe(Effect.orElseSucceed(() => undefined))
    if (info === undefined) return []
    if (info.type === "File") return [{ name: relative, file: absolute }]
    if (info.type !== "Directory") return []
    const found: Array<Input> = []
    for (const entry of yield* fs.readDirectory(absolute).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))) {
      found.push(...(yield* filesUnder(fs, root, path.join(relative, entry), true)))
    }
    return found
  })

/**
 * The contents of `paths` under `root`, and of `extra`, as one sha-256. Names
 * ride with the bytes and the list is sorted, so the hash is a function of the
 * files and never of the order the filesystem listed them in — two hosts
 * holding the same inputs agree.
 */
export const contentHash = Effect.fn("Fingerprint.contentHash")(function*(
  root: string,
  paths: ReadonlyArray<string>,
  extra: ReadonlyArray<Input> = []
) {
  const fs = yield* FileSystem.FileSystem
  const found: Array<Input> = []
  for (const one of paths) found.push(...(yield* filesUnder(fs, root, one)))
  // Code-unit order, as `Array.prototype.sort` orders names: the same on every host.
  const inputs = [...found, ...extra].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
  const hash = crypto.createHash("sha256")
  for (const input of inputs) {
    hash.update(`${input.name}\n`)
    hash.update(crypto.createHash("sha256").update(yield* fs.readFile(input.file)).digest())
  }
  return hash.digest("hex")
})

/**
 * The census run the cut reads, named by its own file name and never by where
 * this host keeps it, so two hosts holding the same run agree. A host with
 * none contributes nothing here; the cut itself refuses (`cut/census.ts`).
 */
const censusInput = Effect.gen(function*() {
  const roots = yield* Roots.Service
  const file = yield* Census.latest((yield* roots.paths({})).census)
  return file === undefined ? [] : [{ name: `census/${path.basename(file)}`, file }]
})

let extractorHeld: string | undefined
let cutHeld: string | undefined

/** What decides an extraction: the deployment's extraction paths. */
export const extractor = Effect.suspend(() =>
  extractorHeld !== undefined
    ? Effect.succeed(extractorHeld)
    : Effect.gen(function*() {
      const decides = yield* Effect.flatMap(Tools.Service, (tools) => tools.decides)
      extractorHeld = yield* contentHash(decides.root, decides.extraction)
      return extractorHeld
    })
)

/** What decides a cut: the deployment's cut paths and the census run it reads. */
export const cut = Effect.suspend(() =>
  cutHeld !== undefined
    ? Effect.succeed(cutHeld)
    : Effect.gen(function*() {
      const decides = yield* Effect.flatMap(Tools.Service, (tools) => tools.decides)
      cutHeld = yield* contentHash(decides.root, decides.cut, yield* censusInput)
      return cutHeld
    })
)

/** A fingerprint as a report line shows it. */
export const short = (fingerprint: string): string => fingerprint.slice(0, 12)
