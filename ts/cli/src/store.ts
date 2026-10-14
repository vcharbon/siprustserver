/**
 * The STORE a command reads and writes: `@sip/store`'s services over the
 * session tree the line's roots name, beside the deployment's layer — which
 * provides what only a deployment states: the database client, the store
 * binding, the population, and every `@sip/corpus` seam. A command that reads
 * the queue takes this, built from its own roots once the line has parsed.
 */
import type { Overrides } from "@sip/corpus/paths"
import {
  Calls,
  CaseDocuments,
  CaseIds,
  CaseMaps,
  Citations,
  Cuts,
  Delta,
  Jobs,
  Measure,
  OpenPreps,
  Pins,
  Refusals,
  Removal,
  Requests,
  RuleHits,
  Runs,
  RunSets,
  Sessions,
  Sources,
  StoreBinding,
  Sweeps,
  Wipe
} from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { pathsOf } from "./options.js"

/** Every store service a command may read, over the client, binding and sessions beneath it. */
export const services = Layer.provideMerge(
  Requests.layer,
  Layer.mergeAll(
    Calls.layer,
    CaseIds.layer,
    CaseMaps.layer,
    Citations.layer,
    Cuts.layer,
    Jobs.layer,
    OpenPreps.layer,
    Pins.layer,
    Wipe.layer,
    Measure.layer,
    Refusals.layer,
    Removal.layer,
    StoreBinding.resolver,
    Runs.layer,
    RunSets.layer,
    CaseDocuments.layer,
    RuleHits.layer,
    Sources.layer,
    Sweeps.layer,
    Delta.layer
  )
)

/** The session tree the line's roots name, over the deployment's `Roots`. */
const sessionsOf = (overrides: Overrides) =>
  Layer.unwrap(Effect.map(pathsOf(overrides), (paths) => Sessions.layer(paths.sessions)))

/** The store over the line's roots, the deployment's layer beneath it and kept in the context. */
export const layer = <ROut, E, RIn>(overrides: Overrides, deployment: Layer.Layer<ROut, E, RIn>) =>
  services.pipe(Layer.provideMerge(sessionsOf(overrides)), Layer.provideMerge(deployment))
