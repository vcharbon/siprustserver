/**
 * A deployment's IDENTITY (`@sip/corpus` `Identity`): a run is recorded under
 * the environment name the pack's `environment` variable states, on the
 * checkout's branch, and stamped by each repository the pack names, under
 * the component name every run row carries.
 */
import { Identity } from "@sip/corpus"
import * as Layer from "effect/Layer"
import type { LabPack } from "./lab-pack.js"

/** A repository a run is stamped by: its component name and its checkout. */
export interface Repository {
  readonly name: string
  readonly dir: string
}

/** The identity, read off this process's environment when the layer is built. */
export const layer = (pack: Pick<LabPack, "checkout" | "variables">, repositories: ReadonlyArray<Repository>) =>
  Layer.sync(Identity.Service, () => {
    const stated = process.env[pack.variables.environment]
    return Identity.Service.of({
      environment: stated === undefined || stated === "" ? null : stated,
      checkout: pack.checkout,
      repositories: [...repositories]
    })
  })
