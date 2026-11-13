/**
 * The base's lanes for a pack that registers no system of its own: the lane
 * catalog over the pack's lane files read as the base reads them, and the
 * hosting of a campaign batch (`@sip/campaign-runner` `LaneHosting`). The
 * simulated lane, `simulated`, runs each cell as one process of the plain
 * replay binary ({@link replayBinaryOf}): the B2BUA worker routing every call
 * to the cell's egress, its records exported under `call-records`. The base
 * stands no hosted lane: a batch on one is refused naming it.
 */
import type { Cell } from "@sip/campaign-runner/cell"
import type { LaneHosting } from "@sip/campaign-runner/replay"
import { LaneHosting as Orchestrated } from "@sip/corpus"
import type { LaneCatalog, LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

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

/** The lane block of a simulated cell's run-spec: the plain system dials the b-leg at the cell's egress endpoint. */
export const laneBlockOf = (cell: Pick<Cell, "egress">) => ({ kind: SIMULATED, egress_endpoint: cell.egress })

/** Why the base does not drive `found`, or null: it stands no hosted lane. */
export const refusalOf = (found: LaneView.View): string | null =>
  found.kind === "simulated"
    ? null
    : `${found.id}: the base stands no hosted lane (its system under test is a pack's to stand); only ${SIMULATED} runs`

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

/**
 * What the lanes of `catalog` are to the orchestrator (`@sip/corpus`
 * `LaneHosting`): the simulated lane builds its system in each cell's process
 * (`NEUTRAL_LANE`); a hosted lane is real-clock and exclusive, and the base
 * stands neither its batch process nor its system.
 */
export const deploymentOf = (catalog: LaneCatalog.Catalog): Orchestrated.Interface => {
  const ids = catalog.lanes.map((lane) => lane.id)
  const unstated = (lane: string) => new Orchestrated.LaneUnstated({ lane, known: ids })
  const unready = (lane: string) => new Orchestrated.LaneUnready({ lane, detail: "the base stands no hosted lane" })
  const kindOf = (lane: string) => catalog.lanes.find((one) => one.id === lane)?.kind
  return {
    lanes: ids,
    lane: (lane) => {
      const kind = kindOf(lane)
      if (kind === undefined) return Effect.fail(unstated(lane))
      return Effect.succeed(kind === "simulated" ? Orchestrated.NEUTRAL_LANE : { clock: "real", exclusive: true })
    },
    standBatch: (lane) => (kindOf(lane) === "simulated" ? Effect.succeed(undefined) : Effect.fail(unready(lane))),
    standSut: (lane) => (kindOf(lane) === "simulated" ? Effect.succeed(undefined) : Effect.fail(unready(lane))),
    preflight: (lane) => {
      const kind = kindOf(lane)
      if (kind === undefined) return Effect.fail(unstated(lane))
      return Effect.succeed(kind === "simulated" ? null : `${lane}: the base stands no hosted lane`)
    }
  }
}

/** The base's hosting of a campaign batch, its simulated cells run by the replay binary at `binary`. */
export const hostingOf = (binary: string): LaneHosting<Cell, LaneView.View, FileSystem.FileSystem> => ({
  prefix: "",
  needs: [],
  laneBlock: (_found, _routing, cell) => laneBlockOf(cell),
  provisioned: () => null,
  sutVariables: () => undefined,
  bundled: () => Effect.void,
  ready: readyOf(binary),
  command: binary,
  box: () => Effect.succeed({}),
  refusal: (found) => Effect.succeed(refusalOf(found))
})

export * as BaseLaneHosting from "./base-lane-hosting.ts"
