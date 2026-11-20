/**
 * What a base campaign run (`base-campaign.ts`) runs on: the store's tables a
 * run writes (its run rows, its job, its batches, their attachments and
 * verification lines, the campaign record) over the client `client` builds,
 * the session tree its bundles are written under, and the identity it is
 * recorded under: its environment, the base's checkout and the pack's tree,
 * each stamped by its commit.
 */
import { Identity } from "@sip/corpus"
import { Roots } from "@sip/lab-host"
import { LAB } from "./base-site-pack.ts"
import * as Pack from "@sip/rig/pack"
import { Attachments, Batches, CampaignRuns, CellAnswers, JobEvents, Jobs, Runs, Sessions, Verifications } from "@sip/store"
import * as Layer from "effect/Layer"
import type { SqlClient } from "effect/unstable/sql/SqlClient"

/** Who records a base campaign run. */
export interface Recorder {
  /** The environment the run is recorded under. */
  readonly env: string
  /** The base's checkout, recorded as `siprig`. */
  readonly checkout: string
  /** The pack's tree, recorded under the pack's component name. */
  readonly pack: { readonly dir: string; readonly manifest: Pack.Pack }
}

/** The store's tables over `client`, the session tree at `sessions`, and `recorder`'s identity. */
export const layerOf = <E, R>(client: Layer.Layer<SqlClient, E, R>, sessions: string, recorder: Recorder) =>
  Layer.mergeAll(
    Roots.layer(LAB),
    Identity.layerWith({
      environment: recorder.env,
      checkout: recorder.checkout,
      repositories: [
        { name: "siprig", dir: recorder.checkout },
        { name: Pack.componentOf(recorder.pack.manifest), dir: recorder.pack.dir }
      ]
    }),
    Layer.mergeAll(
      Runs.layer,
      Jobs.layer,
      JobEvents.layer,
      Batches.layer,
      Attachments.layer,
      CellAnswers.layer,
      Verifications.layer,
      CampaignRuns.layer
    ).pipe(Layer.provideMerge(Sessions.layer(sessions)), Layer.provideMerge(client))
  )

export * as BaseCampaignServices from "./base-campaign-services.ts"
