/**
 * The base's lanes for a pack that registers no system of its own: the lane
 * catalog over the pack's lane files read as the base reads them, and the
 * hosting of a campaign batch (`@sip/campaign-runner` `LaneHosting`). The
 * simulated lane, `simulated`, runs each cell as one process of the plain
 * replay binary ({@link replayBinaryOf}): the B2BUA worker routing every call
 * to the cell's egress, its records exported under `call-records`. A hosted
 * lane runs the upstream B2BUA runner on the real clock, its cells in the
 * plain binary's batch process (`base-hosted-lane.ts`).
 */
import { NodeServices } from "@effect/platform-node"
import type { Cell } from "@sip/campaign-runner/cell"
import type { LaneHosting } from "@sip/campaign-runner/replay"
import { LaneHosting as Orchestrated } from "@sip/corpus"
import { laneOf } from "@sip/lab-host/lane-hosting"
import type { LaneCatalog, LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { hostedRefusalOf, standBatch, standSut } from "./base-hosted-lane.ts"

/** The simulated lane's id: the plain system's lane name. */
export const SIMULATED = "simulated"

/** The variable naming the replay binary a simulated cell runs, ahead of any other place. */
export const REPLAY_VARIABLE = "REPLAY_BIN"

/** The variable naming the directory of prebuilt binaries, where the image puts the plain binary as `replay`. */
export const BIN_DIR_VARIABLE = "SIPRIG_BIN_DIR"

/**
 * The plain replay binary `env` names: {@link REPLAY_VARIABLE}, else `replay`
 * in the {@link BIN_DIR_VARIABLE} directory, else the release build of
 * `pivot-replay` in `checkout`.
 */
export const replayBinaryOf = (env: NodeJS.ProcessEnv, checkout: string): string => {
  const named = env[REPLAY_VARIABLE]
  if (named !== undefined && named !== "") return path.resolve(named)
  const dir = env[BIN_DIR_VARIABLE]
  if (dir !== undefined && dir !== "") return path.join(path.resolve(dir), "replay")
  return path.join(checkout, "target", "release", "pivot-replay")
}

/** How the base reads the lanes of the pack at `pack`: every lane as its view states it, no timer of an in-process system. */
export const readingOf = (pack: string): LaneCatalog.Reading<LaneView.Simulated, LaneView.Hosted, never> => ({
  pack,
  simulatedId: SIMULATED,
  simulatedTimers: {},
  simulated: (lane) => Effect.succeed(lane),
  hosted: (lane) => Effect.succeed(lane)
})

/**
 * The lane block of a cell's run-spec on `found`: the plain system dials the
 * b-leg at the cell's egress endpoint, in process on the simulated lane, at
 * the hosted lane's ingress on a hosted one.
 */
export const laneBlockOf = (found: LaneView.View, cell: Pick<Cell, "egress">) =>
  found.kind === "simulated"
    ? { kind: SIMULATED, egress_endpoint: cell.egress }
    : { kind: "hosted", name: found.id, ingress: found.sip.ingress, egress_endpoint: cell.egress }

/** Why the base does not drive `found`, or null. */
export const refusalOf = (found: LaneView.View): string | null => (found.kind === "simulated" ? null : hostedRefusalOf(found))

/** The replay binary at `binary` made ready: it exists as a file, or the run is told which variable names another. */
const readyOf = (binary: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const info = yield* fs.stat(binary).pipe(Effect.option)
    if (info._tag === "None" || info.value.type !== "File") {
      return yield* Effect.fail(new Error(`no replay binary at ${binary}: build pivot-replay, or name one with ${REPLAY_VARIABLE}`))
    }
    return binary
  })

/** The binaries a campaign's lanes run. */
export interface Binaries {
  /** The plain replay binary. */
  readonly replay: string
  /** The upstream B2BUA runner a managed hosted lane stands. */
  readonly runner: string
}

/**
 * What the lanes of `catalog` are to the orchestrator (`@sip/corpus`
 * `LaneHosting`): the simulated lane builds its system in each cell's process
 * (`NEUTRAL_LANE`); a hosted lane is what its SIP target states
 * (`@sip/lab-host` `laneOf`), its batch process the replay binary's, its
 * system the B2BUA runner, a lane the base cannot host refused before it is
 * driven.
 */
export const deploymentOf = (catalog: LaneCatalog.Catalog, binaries: Binaries): Orchestrated.Interface => {
  const ids = catalog.lanes.map((lane) => lane.id)
  const stated = (lane: string) => {
    const found = catalog.lanes.find((one) => one.id === lane)
    return found === undefined ? Effect.fail(new Orchestrated.LaneUnstated({ lane, known: ids })) : Effect.succeed(found)
  }
  const unready = (lane: string) => (cause: unknown) =>
    new Orchestrated.LaneUnready({ lane, detail: cause instanceof Error ? cause.message : String(cause) })
  return {
    lanes: ids,
    lane: (lane) => Effect.map(stated(lane), laneOf),
    standBatch: (lane, request) =>
      Effect.flatMap(stated(lane), (found) =>
        found.kind === "simulated"
          ? Effect.succeed(undefined)
          : standBatch(binaries.replay, found, request.under).pipe(
            Effect.mapError(unready(lane)),
            Effect.provide(NodeServices.layer)
          )),
    standSut: (lane, under, variables) =>
      Effect.flatMap(stated(lane), (found) =>
        found.kind === "simulated"
          ? Effect.succeed(undefined)
          : standSut(binaries.runner, found, under, variables).pipe(
            Effect.mapError(unready(lane)),
            Effect.provide(NodeServices.layer)
          )),
    preflight: (lane) => Effect.map(stated(lane), refusalOf)
  }
}

/** The base's hosting of a campaign batch, its cells run by the replay binary at `binary`. */
export const hostingOf = (binary: string): LaneHosting<Cell, LaneView.View, FileSystem.FileSystem> => ({
  prefix: "",
  needs: [],
  laneBlock: (found, _routing, cell) => laneBlockOf(found, cell),
  provisioned: () => null,
  sutVariables: () => undefined,
  bundled: () => Effect.void,
  ready: readyOf(binary),
  command: binary,
  box: () => Effect.succeed({}),
  refusal: (found) => Effect.succeed(refusalOf(found))
})

export * as BaseLaneHosting from "./base-lane-hosting.ts"
