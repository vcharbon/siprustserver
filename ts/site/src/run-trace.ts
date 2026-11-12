/**
 * The RECORDED TRACES of one cell: the files of its bundle that hold what went
 * over the wire, listed, and the bytes of one of them.
 *
 * A trace is addressed by the name this listing gave it — a path relative to
 * the cell directory — and a name the listing does not hold is a 404, so no
 * request can address a file outside the bundle.
 *
 * The simulated lane records no pcap: its wire is the per-leg recording and the
 * served HTTP exchange, and those are what it names.
 */
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { NotFound } from "./catalog.ts"
import { ViewerConfig } from "./config.ts"
import { areSegments } from "./segment.ts"
import * as RunDirs from "./run-dirs.ts"
import { captureStem } from "./triage.ts"

/** One recorded trace of a cell, as the bundle holds it. */
export interface Trace {
  /** The file's path inside the cell directory: what the read takes back. */
  readonly file: string
  readonly bytes: number
  /** What the file records: `sip` per-leg wire, `http` the served exchanges, `pcap` a capture, `log` a SUT log the lane kept. */
  readonly kind: "sip" | "http" | "pcap" | "log"
}

export interface Interface {
  /** Every trace of one cell, in name order; a cell with none answers []. */
  readonly list: (
    capture: string,
    run: string,
    cell: string
  ) => Effect.Effect<readonly Trace[], NotFound>
  /** One trace's bytes, by the name the listing gave it. */
  readonly read: (
    capture: string,
    run: string,
    cell: string,
    file: string
  ) => Effect.Effect<Uint8Array, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/RunTrace") {}

/** The bundle file holding the run's served HTTP exchanges. */
const HTTP_FILE = "http.json"

/** A pcap suffix a lane may have left beside the bundle. */
const PCAP = /\.(pcap|pcapng)(\.gz)?$/

/** A log a lane kept in the bundle (the SUT's own, over the cell's batch). */
const LOG = /\.log$/

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service

    const listDir = (dir: string) =>
      fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as string[]))

    const sizeOf = (file: string) =>
      fs.stat(file).pipe(
        Effect.map((info) => Number(info.size)),
        Effect.orElseSucceed(() => undefined)
      )

    /** Where a cell's bundle is, or nothing where the address is not one. */
    const cellDir = Effect.fn("RunTrace.cellDir")(function*(
      capture: string,
      run: string,
      cell: string
    ) {
      const triage = config.triage
      if (triage === undefined || !areSegments(capture, run, cell)) {
        return yield* new NotFound({ id: `${capture}/${run}/${cell}` })
      }
      const runDir = yield* RunDirs.runDirOf(path.join(triage.sessions, captureStem(capture)), run).pipe(
        Effect.provideService(FileSystem, fs),
        Effect.provideService(Path, path)
      )
      const dir = path.join(runDir, cell)
      const held = yield* fs.stat(dir).pipe(
        Effect.map((info) => info.type === "Directory"),
        Effect.orElseSucceed(() => false)
      )
      return held ? dir : yield* new NotFound({ id: `${capture}/${run}/${cell}` })
    })

    const list = Effect.fn("RunTrace.list")(function*(
      capture: string,
      run: string,
      cell: string
    ) {
      const dir = yield* cellDir(capture, run, cell)
      const found: Trace[] = []
      const take = Effect.fn("RunTrace.take")(function*(file: string, kind: Trace["kind"]) {
        const bytes = yield* sizeOf(path.join(dir, file))
        if (bytes !== undefined) found.push({ file, bytes, kind })
      })
      for (const leg of (yield* listDir(path.join(dir, "recording"))).sort()) {
        if (leg.endsWith(".jsonl")) yield* take(`recording/${leg}`, "sip")
      }
      yield* take(HTTP_FILE, "http")
      for (const entry of (yield* listDir(dir)).sort()) {
        if (PCAP.test(entry)) yield* take(entry, "pcap")
        else if (LOG.test(entry)) yield* take(entry, "log")
      }
      return found
    })

    const read = Effect.fn("RunTrace.read")(function*(
      capture: string,
      run: string,
      cell: string,
      file: string
    ) {
      const traces = yield* list(capture, run, cell)
      if (!traces.some((trace) => trace.file === file)) {
        return yield* new NotFound({ id: `${capture}/${run}/${cell}/${file}` })
      }
      const dir = yield* cellDir(capture, run, cell)
      return yield* fs.readFile(path.join(dir, file)).pipe(
        Effect.mapError(() => new NotFound({ id: `${capture}/${run}/${cell}/${file}` }))
      )
    })

    return Service.of({ list, read })
  })
)

export * as RunTrace from "./run-trace.ts"
