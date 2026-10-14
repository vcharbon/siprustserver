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
 * checkout into an unreadable hash error. A command reads each fingerprint
 * once per process for the inputs its deployment states — a campaign's
 * thousands of captures share one tree — while a long-lived reader (the
 * viewer) reads it now, so an edit shows. Either way a file is read again
 * only where its size or mtime moved, so a reading now costs a stat per file.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
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
    hash.update(yield* digestOf(fs, input.file))
  }
  return hash.digest("hex")
})

/** Each file's digest this process took, by path, with the size and mtime it was taken at. */
const digests = new Map<string, { readonly size: bigint; readonly mtime: number; readonly digest: Buffer }>()

/** `file`'s sha-256, read again only where its size or mtime moved since this process last read it. */
const digestOf = Effect.fn("Fingerprint.digestOf")(function*(fs: FileSystem.FileSystem, file: string) {
  const info = yield* fs.stat(file)
  const size = BigInt(info.size)
  const mtime = Option.match(info.mtime, { onNone: () => -1, onSome: (at) => at.getTime() })
  const held = digests.get(file)
  if (held !== undefined && held.size === size && held.mtime === mtime && mtime !== -1) return held.digest
  const digest = crypto.createHash("sha256").update(yield* fs.readFile(file)).digest()
  digests.set(file, { size, mtime, digest })
  return digest
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

/** The fingerprints this process read, by the inputs that decided them. */
const held = new Map<string, string>()

/** `read`, once per process for the inputs `key` names. */
const once = <E, R>(key: string, read: Effect.Effect<string, E, R>): Effect.Effect<string, E, R> =>
  Effect.suspend(() => {
    const hit = held.get(key)
    return hit !== undefined ? Effect.succeed(hit) : Effect.tap(read, (hash) => Effect.sync(() => held.set(key, hash)))
  })

const keyOf = (phase: string, root: string, paths: ReadonlyArray<string>, extra: ReadonlyArray<Input> = []) =>
  JSON.stringify([phase, root, paths, extra])

const decided = Effect.flatMap(Tools.Service, (tools) => tools.decides)

/** What decides an extraction, read now: for a long-lived reader, which must see an edit. */
export const extractorNow = Effect.flatMap(decided, (decides) => contentHash(decides.root, decides.extraction))

/** What decides a cut, read now: for a long-lived reader, which must see an edit. */
export const cutNow = Effect.gen(function*() {
  const decides = yield* decided
  return yield* contentHash(decides.root, decides.cut, yield* censusInput)
})

/** What decides an extraction: the deployment's extraction paths, read once per process. */
export const extractor = Effect.flatMap(decided, (decides) =>
  once(keyOf("extraction", decides.root, decides.extraction), contentHash(decides.root, decides.extraction)))

/** What decides a cut: the deployment's cut paths and the census run it reads, read once per process. */
export const cut = Effect.gen(function*() {
  const decides = yield* decided
  const census = yield* censusInput
  return yield* once(keyOf("cut", decides.root, decides.cut, census), contentHash(decides.root, decides.cut, census))
})

/** A fingerprint as a report line shows it. */
export const short = (fingerprint: string): string => fingerprint.slice(0, 12)
