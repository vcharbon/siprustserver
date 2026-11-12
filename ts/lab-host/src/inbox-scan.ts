/**
 * The INBOX read: which captures the files of a workspace's `inbox/` come to,
 * and which of them have settled. A capture is one file, or the files of one
 * `tcpdump -C` ring (`tap.pcap`, `tap.pcap1`, `tap.pcap2.gz`…), in ring order.
 * A dotfile (an `rsync` partial, a tool's own scratch) and a name that is no
 * capture form are not captures. A file is settled once two reads in a row
 * find it at the same size and modification time: a copy still being written
 * changes between them.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** One inbox file as a read found it. */
export interface Entry {
  readonly name: string
  readonly size: number
  readonly mtimeMs: number
}

/** One capture of the inbox: its files in the order its reader takes them. */
export interface Capture {
  /** The stem the anonymised copy is named after: the first file's name up to `.pcap`. */
  readonly stem: string
  readonly files: ReadonlyArray<Entry>
}

/** A capture form, numbered as a ring or not: `@sip/corpus` `Discover.isRingMember`. */
const RING_MEMBER = /\.pcap(ng)?(\.gz)?(\d*)(\.gz)?$/i

/** Name order with numbers compared by value: `tap.pcap9` before `tap.pcap10`. */
const natural = new Intl.Collator("en", { numeric: true })

/** Whether `name` is an inbox capture file: no dotfile, a capture form. */
export const isCaptureFile = (name: string): boolean => !name.startsWith(".") && RING_MEMBER.test(name)

/** The stem an anonymised copy of a capture starting at `name` takes: the name up to its first `.pcap`. */
export const stemOf = (name: string): string => {
  const at = name.indexOf(".pcap")
  return at < 0 ? name : name.slice(0, at)
}

/** The ring a file belongs to and its place in it: the name with its ring number off, and the number. */
const ringOf = (name: string): { readonly ring: string; readonly index: string } => {
  const match = RING_MEMBER.exec(name)
  if (match === null) return { ring: name, index: "" }
  return { ring: name.slice(0, match.index) + `.pcap${match[1] ?? ""}`, index: match[3] ?? "" }
}

/**
 * The captures `entries` come to: the files of one ring together, in ring
 * order; a ring holding two files at one place (`tap.pcap` beside
 * `tap.pcap.gz`) is no ring, and each of its files is a capture of its own.
 */
export const captures = (entries: ReadonlyArray<Entry>): ReadonlyArray<Capture> => {
  const rings = new Map<string, Array<Entry>>()
  for (const entry of entries) {
    if (!isCaptureFile(entry.name)) continue
    const { ring } = ringOf(entry.name)
    const members = rings.get(ring) ?? []
    members.push(entry)
    rings.set(ring, members)
  }
  const out: Array<Capture> = []
  for (const members of rings.values()) {
    const places = new Set(members.map((member) => ringOf(member.name).index))
    const groups = places.size === members.length ? [members] : members.map((member) => [member])
    for (const group of groups) {
      const files = [...group].sort((a, b) => natural.compare(a.name, b.name))
      out.push({ stem: stemOf(files[0]!.name), files })
    }
  }
  return out.sort((a, b) => natural.compare(a.files[0]!.name, b.files[0]!.name))
}

/** What tells one state of a file from another: its size and modification time. */
export const stamp = (entry: Entry): string => `${entry.size}:${entry.mtimeMs}`

/** Whether every file of `capture` stands as `before` last found it. */
export const settled = (capture: Capture, before: ReadonlyMap<string, string>): boolean =>
  capture.files.every((file) => before.get(file.name) === stamp(file))

/** The files directly under `dir` with their stats; a dir that does not read holds none. */
export const read = Effect.fn("InboxScan.read")(function*(dir: string) {
  const fs = yield* FileSystem.FileSystem
  const names = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
  const entries: Array<Entry> = []
  for (const name of names) {
    if (!isCaptureFile(name)) continue
    const info = yield* fs.stat(path.join(dir, name)).pipe(Effect.option)
    if (info._tag === "None" || info.value.type !== "File") continue
    const mtime = info.value.mtime
    entries.push({
      name,
      size: Number(info.value.size),
      mtimeMs: mtime._tag === "Some" ? mtime.value.getTime() : 0
    })
  }
  return entries as ReadonlyArray<Entry>
})
