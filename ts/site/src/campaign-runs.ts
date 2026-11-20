/**
 * The CAMPAIGN RUNS the site reads, as its pack's store holds them: each a
 * campaign run (`@sip/store` `CampaignResults`) with what the pack
 * states of it beside it — the labels its header shows after its
 * environment, branch and seed, what each case it compiled says of itself,
 * and, where its seed left objects, the link of each. A case result's kept
 * cell is named by the lane run's bundle it lies under. The pack fills the
 * service (`Service`) over its own store.
 */
import type { Checked } from "@sip/campaign-runner/cell"
import type { CampaignResults as Results } from "@sip/store"
import * as Context from "effect/Context"
import type * as Effect from "effect/Effect"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** What a case says of itself, as the newest run that compiled it states it. */
export interface About {
  readonly title: string
  readonly description: string | null
  /** Its labels, each once, in the order the pack states them. */
  readonly groups: ReadonlyArray<string>
  /** The campaign tags it carries. */
  readonly tags: ReadonlyArray<string>
  /** The case file it was compiled from; null where the run kept none. */
  readonly source: { readonly file: string; readonly text: string } | null
  /** What it uses, by role and label; `object` names the seed's object a link is made to, null where none is. */
  readonly uses: ReadonlyArray<{ readonly role: string; readonly label: string; readonly object: string | null }>
  /** What each sub-case checks, passing or not, by sub-case; {} where the run kept none. */
  readonly checked: Readonly<Record<string, ReadonlyArray<Checked>>>
}

/** A campaign run as the site reads it. */
export interface Run extends Results.CampaignRun {
  /** The labels its header shows after its environment, branch and seed, by name; a null value is not shown. */
  readonly labels: ReadonlyArray<readonly [string, string | null]>
  /** What each case it compiled says of itself, by `ref`. */
  readonly about: Readonly<Record<string, About>>
  /** The link of the object `name` its seed left, on `base` where one is stated; absent where the run was not seeded with links. */
  readonly linkOf?: ((name: string, base: string | undefined) => string | null) | undefined
}

/** A case result's kept cell: its directory under the lane run's bundle, the case result it is, and its run row's lane. */
export interface KeptCell {
  readonly cell: string
  readonly runId: number
  readonly caseId: string
  readonly lane: string
}

export interface ReadOptions {
  readonly limit: number
  readonly env?: string
  readonly include?: number
  /** Only the partial runs (`true`) or only the full ones (`false`); absent, both. */
  readonly partial?: boolean
}

export interface Interface {
  /** Every campaign the store holds a run of. */
  readonly captures: Effect.Effect<ReadonlyArray<string>, SqlError>
  /** The campaign's latest `limit` runs, newest first (of `env` where named, partial or full where `partial` says), the run `include` names among them whatever its age. */
  readonly read: (capture: string, options: ReadOptions) => Effect.Effect<ReadonlyArray<Run>, SqlError>
  /** The environments that wrote the campaign's runs, the latest writer first. */
  readonly envs: (capture: string) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /** Every case result of the lane run whose bundle is `bundlePath`: its kept cell, by name. */
  readonly keptCells: (bundlePath: string) => Effect.Effect<ReadonlyArray<KeptCell>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/CampaignRuns") {}

export * as CampaignRuns from "./campaign-runs.ts"
