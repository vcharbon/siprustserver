/**
 * A file of the workspace's `pcaps/` replayed on a lane, as one action: an
 * INGEST job over the file that cuts what it leaves stale, then a REPLAY job
 * launched with `--after` it, over a request meaning the capture whole — a
 * request made before the cut reaches the capture, which grows to every call
 * the cut makes of it once the replay resolves it (`Request.ahead`). Both
 * jobs are in the jobs list from the start; the replay's row says it waits on
 * the ingest. A lane a capture replay does not fit runs it only when forced,
 * its runs then recorded forced; one with no harness on its SUT's routing
 * path runs nothing, forced or not (`pcap-lanes.ts`). A lane the pack's lab
 * guard refuses here (in a workspace, one naming another lab's container) is
 * refused.
 */
import * as Discover from "@sip/corpus/ingest/discover"
import * as Extract from "@sip/corpus/ingest/extract"
import * as JobStart from "@sip/corpus/jobs/job-start"
import * as Request from "@sip/corpus/requests/request"
import { LaneCatalog } from "@sip/rig"
import { Jobs, Requests } from "@sip/store"
import type { Layout as WorkspaceLayout } from "@sip/workspace"
import { Effect, Schema } from "effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { DEFAULT_KNOBS, knobsOn } from "./lanes.ts"
import type { Paths } from "./paths.ts"
import { PcapLanes } from "./pcap-lanes.ts"
import type { SitePack } from "./site-pack.ts"

export interface Ask {
  /** A file name of `pcaps/`, no path. */
  readonly file: string
  /** The lane, by its catalog id. */
  readonly lane: string
  /** Drive a lane a capture replay does not fit; its runs are recorded forced. */
  readonly force: boolean
}

export interface Started {
  readonly id: number
  readonly kind: Jobs.Kind
  readonly label: string
  readonly logPath: string
}

export class Refused extends Schema.TaggedError<Refused>()("Site.PcapReplayRefused", { detail: Schema.String }) {}

/** Why `ask` names no file of `pcaps` and no lane the replay may run on, or the lane and the absolute file. */
const checked = Effect.fn("PcapReplay.checked")(function*(
  workspace: WorkspaceLayout.Base | undefined,
  pack: Pick<SitePack.Pack, "lanes" | "laneRefusal">,
  ask: Ask
) {
  const catalog = pack.lanes
  if (workspace === undefined) return yield* new Refused({ detail: "no workspace: there is no pcap root to replay from" })
  if (ask.file === "" || ask.file.includes("/") || ask.file.includes("\\") || ask.file.startsWith(".")) {
    return yield* new Refused({ detail: `${JSON.stringify(ask.file)} is no file name of ${workspace.pcaps}` })
  }
  const file = path.join(workspace.pcaps, ask.file)
  const fs = yield* FileSystem.FileSystem
  const info = yield* fs.stat(file).pipe(Effect.option)
  if (info._tag === "None" || info.value.type !== "File") {
    return yield* new Refused({ detail: `${file} is no file` })
  }
  const lane = LaneCatalog.find(catalog, ask.lane)
  if (lane === undefined) {
    return yield* new Refused({ detail: `no lane ${ask.lane} (lanes: ${LaneCatalog.ids(catalog).join(", ")})` })
  }
  const fit = PcapLanes.fitOf(lane)
  if (fit.fit === "none" || (fit.fit === "forced" && !ask.force)) {
    return yield* new Refused({
      detail: fit.fit === "none"
        ? fit.line ?? `${lane.id} runs no capture replay`
        : `a capture replay is not runnable on this lane: ${lane.id} lacks ${fit.lacks.join(", ")} (force runs it anyway, marked forced)`
    })
  }
  const foreign = yield* pack.laneRefusal(lane, process.env)
  if (foreign !== null) return yield* new Refused({ detail: foreign })
  return { file, lane: lane.id, forced: fit.fit === "forced" }
})

/** The two jobs, launched in order: the ingest + cut, then the replay waiting for it. */
export const launch = Effect.fn("PcapReplay.launch")(function*(
  paths: Paths,
  workspace: WorkspaceLayout.Base | undefined,
  pack: Pick<SitePack.Pack, "lanes" | "laneRefusal" | "censusRefusal">,
  ask: Ask
) {
  const { file, lane, forced } = yield* checked(workspace, pack, ask)
  const catalog = pack.lanes
  const census = yield* pack.censusRefusal(paths)
  if (census !== null) return yield* new Refused({ detail: census })
  const requests = yield* Requests.Service
  // The generation the file's capture stands at in the store: the name the Pcaps page shows for it.
  const capture = (yield* Extract.standing(paths.corpus, { capture: Discover.captureOf(file), pcaps: [file] })).capture
  const notStarted = (error: unknown) =>
    JobStart.isNotStarted(error) ? Effect.fail(new Refused({ detail: error.message })) : Effect.die(error)
  const ingest = yield* JobStart.ingest(paths.sessions, {
    paths: [file],
    set: [],
    captures: [capture],
    cut: true,
    concurrency: DEFAULT_KNOBS.concurrency,
    memoryMax: DEFAULT_KNOBS.memoryMax
  }).pipe(Effect.catch(notStarted))
  const ahead = Request.ahead([capture])
  const created = yield* requests.create({
    lane,
    selection: ahead.selection,
    knobs: knobsOn(catalog, lane, {}),
    createdBy: yield* JobStart.createdBy,
    cells: ahead.cells
  }).pipe(Effect.orDie)
  const row = yield* requests.get(created.id).pipe(Effect.orDie)
  if (row === null) return yield* Effect.die(new Error(`request ${created.id} vanished as it was made`))
  const replay = yield* JobStart.replay(paths.sessions, row, {
    prep: null,
    batch: null,
    concurrency: null,
    memoryMax: null,
    startsPerSecond: null,
    cleanup: row.knobs.cleanup,
    wait: null,
    unran: false,
    after: ingest.id,
    force: forced,
    collection: null
  }).pipe(Effect.catch(notStarted))
  return [
    { id: ingest.id, kind: "ingest", label: `ingest + cut of ${ask.file}`, logPath: ingest.launched.logPath },
    {
      id: replay.id,
      kind: "replay",
      label: `replay of ${capture} on ${lane} (request ${created.id}) after job ${ingest.id}` + (forced ? ", forced" : ""),
      logPath: replay.launched.logPath
    }
  ] satisfies ReadonlyArray<Started>
})

export * as PcapReplay from "./pcap-replay.ts"
