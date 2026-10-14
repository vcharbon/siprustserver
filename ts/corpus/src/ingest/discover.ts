/**
 * Which captures an invocation names: pcap files on the host, gathered into
 * SOURCES — one capture each, or one ordered SET of the consecutive files of
 * one tap.
 *
 * A capture is keyed by its NAME everywhere, so the name a pcap takes is byte
 * for byte what the store already holds, and two pcaps that come to one name
 * are one capture, the first winning. An input that names nothing is reported,
 * never dropped.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { PCAP_SUFFIXES } from "../paths.js"

/**
 * One capture to extract: the ordered files it is read from, and its name —
 * the generation it stands at in the store, `first` naming the first
 * generation where it is a later one.
 */
export interface Source {
  readonly capture: string
  readonly pcaps: ReadonlyArray<string>
  readonly first?: string
}

/**
 * The capture name a pcap takes: a basename with a trailing `.pcap` off — and
 * only `.pcap`, so `x.pcap.gz` keeps its whole name — and every character
 * outside `[A-Za-z0-9._-]` replaced.
 */
export const captureOf = (pcap: string): string => {
  const name = path.basename(pcap)
  const stem = name.endsWith(".pcap") ? name.slice(0, -".pcap".length) : name
  return stem.replace(/[^A-Za-z0-9._-]/g, "_")
}

/**
 * The capture name a set takes: its one file's, or the first file's with the
 * file count — `ring_00000_set3` — so a set and its first file alone are two
 * captures.
 */
export const captureOfSet = (pcaps: ReadonlyArray<string>): string =>
  pcaps.length === 1 ? captureOf(pcaps[0]!) : `${captureOf(pcaps[0]!)}_set${pcaps.length}`

const isPcap = (file: string): boolean => PCAP_SUFFIXES.some((suffix) => file.endsWith(suffix))

/**
 * A file of a ring: a capture suffix, or one with the number `tcpdump -C`
 * appends (`tap.pcap`, `tap.pcap1`, `tap.pcap2`), gzipped after rotation or
 * not (`tap.pcap1.gz`).
 */
export const isRingMember = (file: string): boolean => /\.pcap(ng)?(\.gz)?\d*(\.gz)?$/.test(file)

/**
 * Name order with numbers compared by value: `tap.pcap9` before `tap.pcap10`,
 * as `tcpdump -C` numbers a ring; a ring named by `editcap` or by time sorts
 * the same either way.
 */
const natural = new Intl.Collator("en", { numeric: true })

/** Every `member` file under `dir`, at any depth; a link met inside it is not followed. */
const under = (
  fs: FileSystem.FileSystem,
  dir: string,
  member: (name: string) => boolean
): Effect.Effect<ReadonlyArray<string>, never> =>
  Effect.gen(function*() {
    const found: Array<string> = []
    for (const entry of yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))) {
      const child = path.join(dir, entry)
      if (yield* fs.readLink(child).pipe(Effect.as(true), Effect.orElseSucceed(() => false))) continue
      const info = yield* fs.stat(child).pipe(Effect.orElseSucceed(() => undefined))
      if (info?.type === "Directory") found.push(...(yield* under(fs, child, member)))
      else if (info?.type === "File" && member(entry)) found.push(child)
    }
    return found
  })

/**
 * The files named on the host, each input resolved against `cwd` — the
 * OPERATOR's directory, never the pipeline's — a directory contributing every
 * `member` under it, a file itself whatever it is called.
 */
const named = (inputs: ReadonlyArray<string>, cwd: string, member: (name: string) => boolean) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const files: Array<string> = []
    const missing: Array<string> = []
    for (const input of inputs) {
      const absolute = path.resolve(cwd, input)
      const info = yield* fs.stat(absolute).pipe(Effect.orElseSucceed(() => undefined))
      if (info === undefined) {
        missing.push(`${input} names nothing at ${absolute}`)
        continue
      }
      if (info.type === "Directory") files.push(...(yield* under(fs, absolute, member)))
      else files.push(absolute)
    }
    return { files, missing }
  })

/** What an invocation's inputs came to: its sources, the names that collided, the inputs that named nothing. */
export interface Discovered {
  readonly sources: ReadonlyArray<Source>
  readonly collisions: ReadonlyArray<string>
  readonly missing: ReadonlyArray<string>
}

/**
 * The one capture `--set` names: every file it lists, a directory's ring
 * members included, in ring order. The extractor decides whether they are
 * consecutive; here they are only gathered and named.
 */
export interface SetFound {
  readonly source: Source | undefined
  readonly missing: ReadonlyArray<string>
}

export const set = Effect.fn("Discover.set")(function*(inputs: ReadonlyArray<string>, cwd: string) {
  const { files, missing } = yield* named(inputs, cwd, isRingMember)
  const pcaps = [...files].sort((a, b) => natural.compare(path.basename(a), path.basename(b)))
  return {
    source: pcaps.length === 0 ? undefined : { capture: captureOfSet(pcaps), pcaps },
    missing: missing.map((line) => `--set ${line}`)
  } satisfies SetFound
})

/**
 * The `--pcaps` captures and the `--set` capture as one invocation's sources,
 * under `discover`'s rule: one capture per name, the first wins — a set whose
 * name a `--pcaps` capture holds loses to it, and is named.
 */
export const join = (each: Discovered, set: SetFound): Discovered => {
  if (set.source === undefined) return { ...each, missing: [...each.missing, ...set.missing] }
  const held = each.sources.find((source) => source.capture === set.source?.capture)
  return {
    sources: held === undefined ? [...each.sources, set.source] : each.sources,
    collisions: held === undefined ? each.collisions : [
      ...each.collisions,
      `--set ${set.source.pcaps.join(" ")} names the same capture as ${held.pcaps[0]} (${held.capture})`
    ],
    missing: [...each.missing, ...set.missing]
  }
}

/**
 * The pcaps an invocation named: a directory contributes every pcap under it,
 * a file contributes itself whatever it is called. Relative paths resolve
 * against `cwd`, which is the OPERATOR's directory and not the pipeline's.
 *
 * Two pcaps that come to one capture name are one capture, and the first wins:
 * the store is keyed by the name, so taking both would extract one over the
 * other and record whichever ran last. The loser is named so the collision is
 * visible rather than a count that does not add up.
 */
export const discover = Effect.fn("Discover.discover")(function*(inputs: ReadonlyArray<string>, cwd: string) {
  const { files, missing } = yield* named(inputs, cwd, isPcap)
  const sources = new Map<string, Source>()
  const collisions: Array<string> = []
  for (const pcap of [...files].sort()) {
    const capture = captureOf(pcap)
    const held = sources.get(capture)
    if (held !== undefined) {
      collisions.push(`${pcap} names the same capture as ${held.pcaps[0]} (${capture})`)
      continue
    }
    sources.set(capture, { capture, pcaps: [pcap] })
  }
  return {
    sources: [...sources.values()],
    collisions,
    missing: missing.map((line) => `--pcaps ${line}`)
  } satisfies Discovered
})
