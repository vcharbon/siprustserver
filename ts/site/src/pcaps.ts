/**
 * The Pcaps page's read: every capture file of the workspace's `pcaps/`, with
 * its size and date, where the pipeline stands with it — `new` (never
 * ingested), `ingested` (in the population, not cut), `cut` (with its case
 * count) — and its last full run per lane, whoever made it; the inbox beside it
 * (`inbox.ts`); and how a capture replay fits each lane (`pcap-lanes.ts`).
 * A file is matched to its source row by the path its ingest recorded; the
 * newest generation of that path speaks for it.
 *
 * Outside a workspace the page lists nothing: the inbox and its pcap root are
 * a workspace's.
 */
import { Fingerprint } from "@sip/corpus"
import * as Discover from "@sip/corpus/ingest/discover"
import { Sources } from "@sip/store"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { Inbox } from "./inbox.ts"
import { PcapLanes } from "./pcap-lanes.ts"
import { SitePack } from "./site-pack.ts"

/** Where the pipeline stands with one file. */
export type State = "new" | "ingested" | "cut"

export interface Pcap {
  readonly file: string
  readonly size: number
  readonly modifiedAt: string
  /** The capture the population holds it as; the name its ingest will give it where it is `new`. */
  readonly capture: string
  readonly state: State
  /** The standing cut's cases; null unless `cut`. */
  readonly cases: number | null
  /** current | stale | failed, as the source list judges it; null where `new`. */
  readonly ingest: string | null
  /** current | stale | broke | never; null where `new`. */
  readonly cut: string | null
  /** Why the ingest failed or the cut broke; null otherwise. */
  readonly problem: string | null
  readonly standings: ReadonlyArray<Sources.LaneStanding>
}

export interface View {
  /** The pcap root and the inbox; null outside a workspace. */
  readonly pcaps: string | null
  readonly inbox: string | null
  readonly files: ReadonlyArray<Pcap>
  readonly inboxEntries: ReadonlyArray<Inbox.Entry>
  readonly lanes: ReadonlyArray<PcapLanes.LaneFit>
}

export interface Interface {
  readonly list: Effect.Effect<View>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/PcapList") {}

/** The source rows one read takes: every generation of every file of a pcap root. */
const ROW_CAP = 10_000

/** A capture form as the ingest takes one file (`@sip/corpus` `PCAP_SUFFIXES`). */
const isPcap = (name: string): boolean => !name.startsWith(".") && /\.pcap(ng)?(\.gz)?$/i.test(name)

/** Where the pipeline stands with a file, off its source row. */
export const stateOf = (row: Sources.Row | undefined): State =>
  row === undefined ? "new" : row.prepId === null || row.cut === "broke" || row.cut === "never" ? "ingested" : "cut"

/** One file as the page shows it, off its stat and its source row. */
export const pcapOf = (
  file: { readonly name: string; readonly path: string; readonly size: number; readonly mtimeMs: number },
  row: Sources.Row | undefined
): Pcap => {
  const state = stateOf(row)
  return {
    file: file.name,
    size: file.size,
    modifiedAt: new Date(file.mtimeMs).toISOString(),
    capture: row?.capture ?? Discover.captureOf(file.path),
    state,
    cases: state === "cut" ? row?.cases ?? 0 : null,
    ingest: row?.ingest ?? null,
    cut: row?.cut ?? null,
    problem: row?.ingestFailed ?? row?.cutBroke ?? null,
    standings: row?.standings ?? []
  }
}

/** The newest generation of each source path among `rows`, by path. */
const newestByPath = (rows: ReadonlyArray<Sources.Row>): ReadonlyMap<string, Sources.Row> => {
  const out = new Map<string, Sources.Row>()
  for (const row of rows) {
    if (row.sourcePath === null) continue
    const held = out.get(row.sourcePath)
    if (held === undefined || held.addedAt < row.addedAt) out.set(row.sourcePath, row)
  }
  return out
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const sources = yield* Sources.Service
    const inbox = yield* Inbox.Service
    const pack = yield* SitePack.Service
    const host = yield* Effect.context<Host>()
    const lanes = PcapLanes.fits(pack.lanes)

    const files = Effect.fn("PcapList.files")(function*(dir: string) {
      const fs = yield* FileSystem.FileSystem
      const names = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
      const out: Array<{ name: string; path: string; size: number; mtimeMs: number }> = []
      for (const name of [...names].filter(isPcap).sort()) {
        const one = path.join(dir, name)
        const info = yield* fs.stat(one).pipe(Effect.option)
        if (info._tag === "None" || info.value.type !== "File") continue
        out.push({
          name,
          path: one,
          size: Number(info.value.size),
          mtimeMs: info.value.mtime._tag === "Some" ? info.value.mtime.value.getTime() : 0
        })
      }
      return out
    })

    const list = Effect.gen(function*() {
      const workspace = config.workspace
      if (workspace === undefined) {
        return { pcaps: null, inbox: null, files: [], inboxEntries: [], lanes } satisfies View
      }
      const found = yield* files(workspace.pcaps).pipe(Effect.provide(host))
      const rows = found.length === 0 ? [] : (yield* sources.list({
        sourcePaths: found.map((one) => one.path),
        standings: "latest",
        limit: ROW_CAP,
        offset: 0,
        extractorHash: yield* Fingerprint.extractorNow.pipe(Effect.provide(host), Effect.orDie),
        cutFingerprint: yield* Fingerprint.cutNow.pipe(Effect.provide(host), Effect.orDie)
      }).pipe(Effect.orDie)).rows
      const byPath = newestByPath(rows)
      return {
        pcaps: workspace.pcaps,
        inbox: workspace.inbox,
        files: found.map((one) => pcapOf(one, byPath.get(one.path))),
        inboxEntries: yield* inbox.list,
        lanes
      } satisfies View
    }).pipe(Effect.withSpan("PcapList.list"))

    return Service.of({ list })
  })
)

export * as PcapList from "./pcaps.ts"
