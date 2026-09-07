#!/usr/bin/env -S node --import tsx
/**
 * The driver's own entry point: the neutral composition.
 *
 * It runs a campaign of rust-test cells, and it REFUSES every pivot-replay cell
 * — no lane presets are composed here, and a lane nobody described is a lane
 * this binary must not guess at. A deployment ships its own entry point with its
 * presets layered in; this one exists so `sip-driver run --help` works and so
 * the composition is written down once, in the shape a deployment copies.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import { Classifier, Reclassifier } from "@sip/pipeline"
import { CliUnavailable, ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { Command } from "effect/unstable/cli"
import { driver } from "./cli.js"
import { LanePresets } from "./lanes.js"
import { RoutingCompiler } from "./routing.js"

/**
 * `REPLAY_BIN` is required and has no default, and a missing one must not stop
 * `--help` or a campaign of rust-test cells: the binary is UNAVAILABLE, which is
 * a fact about one cell, not about the program.
 */
const unconfigured = (args: ReadonlyArray<string>) =>
  Effect.fail(
    new CliUnavailable({
      binary: "replay",
      args,
      cause: "REPLAY_BIN names no binary — a pivot-replay cell needs the interpreter's path"
    })
  )

const replay = ReplayCli.layer.pipe(
  Layer.catchCause(() =>
    Layer.succeed(
      ReplayCli.Service,
      ReplayCli.Service.of({
        run: (runSpecPath) => unconfigured([runSpecPath]),
        schema: () => unconfigured(["schema"])
      })
    )
  )
)

const services = Layer.mergeAll(
  replay,
  Reclassifier.layer,
  Classifier.layer,
  LanePresets.layer,
  RoutingCompiler.layer
).pipe(Layer.provideMerge(NodeServices.layer))

Command.run(driver, { version: "0.0.0" }).pipe(Effect.provide(services), NodeRuntime.runMain)
