/**
 * `Identity` — who makes a run here: the environment name the run is recorded
 * under, the checkout whose branch it records, and every repository whose
 * commit, dirty flag and tree fingerprint stamp it, by component name.
 *
 * The environment is recorded as stated: a host that states none records
 * none, and its runs are never the current population's. A comparison of two
 * runs reads their code per component name, so the names are the deployment's
 * vocabulary and stay stable.
 *
 * The default states nothing: no environment, the process's own directory as
 * the checkout, no repository — a run whose code is unknown, said so.
 */
import * as Context from "effect/Context"
import * as Layer from "effect/Layer"

/** One repository the deployment builds from, under the component name its runs record. */
export interface Repository {
  readonly name: string
  readonly dir: string
}

export interface Interface {
  /** The environment name runs are recorded under; `null` where the host states none. */
  readonly environment: string | null
  /** The checkout whose branch a run records. */
  readonly checkout: string
  /** Every repository whose commit stamps a run. */
  readonly repositories: ReadonlyArray<Repository>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Identity") {}

/** The default: an unnamed environment, this directory, no repository. */
export const layer = Layer.sync(Service, () =>
  Service.of({ environment: null, checkout: process.cwd(), repositories: [] }))

/** A deployment's own identity. */
export const layerWith = (identity: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(identity))
