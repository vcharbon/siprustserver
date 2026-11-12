/**
 * The campaigns a deployment runs: those its configuration file defines
 * (`[campaign.<name>]`), read anew at each call so the CLI and the site read
 * one file the same way. A workspace defining none has one campaign of every
 * case under the tooling's default name; a checkout always has that one,
 * beside those its file defines. They share provisioning, lanes, tag
 * declarations and the store; each has its own page and run history, its name
 * being the capture its runs are recorded under. A tooling's own campaign
 * keys are read into its own fields ({@link Definitions.extra}).
 */
import type { Refused } from "@sip/cli/host-env"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import type * as ConfigFile from "./config-file.js"
import type * as DeploymentConfig from "./deployment-config.js"

/** The glob of every case file a run reads. */
export const EVERY_CASE = "**/*.ts"

/** One campaign, its defaults applied. */
export interface Base {
  readonly name: string
  /** Its name on the page, its own name where the file states no title; null for the default campaign, which has none. */
  readonly title: string | null
  /** Globs over case file names. */
  readonly cases: ReadonlyArray<string>
  /** The lanes a run takes when it names none; null for the first runnable lane's group. */
  readonly lanes: ReadonlyArray<string> | null
  /** The tags it opts into by itself. */
  readonly allow: ReadonlyArray<string>
  /** The operations its runs inject after provisioning, in order, and heal in reverse at their end. */
  readonly setup: ReadonlyArray<string>
}

/** How a tooling defines its campaigns: `K` its named campaign as its file reads, `X` the fields it adds. */
export interface Definitions<K extends ConfigFile.NamedCampaign, X extends object> {
  /** The campaign of a deployment defining none. */
  readonly defaultName: string
  /** The tooling's fields of a stated campaign, or of the default one (`undefined`). */
  readonly extra: (campaign: K | undefined) => X
  /** The configuration the deployment `env` names (`deployment-config.ts` `of`). */
  readonly deployment: (
    env: NodeJS.ProcessEnv
  ) => Effect.Effect<DeploymentConfig.Stated<{ readonly campaign?: Readonly<Record<string, K>> }>, Refused, FileSystem.FileSystem>
  /** Whether `env` names a workspace rather than the checkout. */
  readonly inWorkspace: (env: NodeJS.ProcessEnv) => boolean
}

const namesLine = (defined: ReadonlyArray<Base>): string => defined.map((one) => one.name).join(", ")

/** Why no campaign `name` is defined, naming those that are. */
export const unknownReason = (name: string, defined: ReadonlyArray<Base>): string =>
  `no campaign ${name}; the campaigns: ${namesLine(defined)}`

/**
 * A tooling's campaigns, its defaults and fields applied: `DEFAULT`, the
 * campaign of a deployment defining none; `definedFrom`, a file's campaigns
 * in its order, else `DEFAULT`; `definedInCheckout`, `DEFAULT` first, then a
 * checkout's file's; `statedIn`/`definedIn`, those of the deployment `env`
 * names; `chosen`, the campaign a run takes: the one named, else the only one,
 * else `DEFAULT` where a checkout holds it beside its own, or why not.
 */
export const make = <K extends ConfigFile.NamedCampaign, X extends object>(definitions: Definitions<K, X>) => {
  type Defined = Base & X
  const DEFAULT: Defined = {
    name: definitions.defaultName,
    title: null,
    cases: [EVERY_CASE],
    lanes: null,
    allow: [],
    setup: [],
    ...definitions.extra(undefined)
  }
  const definedFrom = (config: { readonly campaign?: Readonly<Record<string, K>> }): ReadonlyArray<Defined> => {
    const stated = Object.entries(config.campaign ?? {})
    return stated.length === 0 ? [DEFAULT] : stated.map(([name, campaign]) => ({
      name,
      title: campaign.title ?? name,
      cases: campaign.cases,
      lanes: campaign.lanes ?? null,
      allow: campaign.allow ?? [],
      setup: campaign.setup ?? [],
      ...definitions.extra(campaign)
    }))
  }
  const definedInCheckout = (config: { readonly campaign?: Readonly<Record<string, K>> }): ReadonlyArray<Defined> => {
    const stated = definedFrom(config)
    return stated.some((one) => one.name === DEFAULT.name) ? stated : [DEFAULT, ...stated]
  }
  const statedIn = (env: NodeJS.ProcessEnv) =>
    Effect.map(definitions.deployment(env), ({ file, config }) => ({
      file,
      defined: definitions.inWorkspace(env) ? definedFrom(config) : definedInCheckout(config)
    }))
  const definedIn = (env: NodeJS.ProcessEnv) => Effect.map(statedIn(env), ({ defined }) => defined)
  const chosen = (defined: ReadonlyArray<Defined>, named: string | undefined): Defined | string => {
    if (named !== undefined) return defined.find((one) => one.name === named) ?? unknownReason(named, defined)
    if (defined.length === 1) return defined[0]!
    return defined.includes(DEFAULT)
      ? DEFAULT
      : `several campaigns are defined, name one with --campaign: ${namesLine(defined)}`
  }
  return { DEFAULT, definedFrom, definedInCheckout, statedIn, definedIn, chosen }
}
