/**
 * The base's CAMPAIGN RUNS (`@sip/site` `campaign-runs.ts`) over the store's
 * campaign record (`@sip/store` `CampaignRuns`): no label after a run's
 * environment, branch and seed, no object link, and what a case says of
 * itself read off the record's `about` where it states a title, its ref
 * standing for one where it does not.
 */
import { CampaignRuns as StoreRuns } from "@sip/store"
import { CampaignRuns } from "@sip/site/campaign-runs"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

/** A string field of `value`, or undefined. */
const stringOf = (value: unknown, key: string): string | undefined => {
  if (typeof value !== "object" || value === null) return undefined
  const field = (value as Record<string, unknown>)[key]
  return typeof field === "string" ? field : undefined
}

/** What the case `ref` says of itself, as the record's `about` holds it. */
export const aboutOf = (ref: string, about: unknown): CampaignRuns.About => ({
  title: stringOf(about, "title") ?? ref,
  description: stringOf(about, "description") ?? null,
  groups: [],
  tags: [],
  source: null,
  uses: [],
  checked: {}
})

/** A stored run as the site reads it. */
export const runOf = (run: StoreRuns.StoredRun): CampaignRuns.Run => ({
  ...run,
  labels: [],
  about: Object.fromEntries(Object.entries(run.about).map(([ref, about]) => [ref, aboutOf(ref, about)]))
})

/** The site's campaign runs over the store's record. */
export const layer = Layer.effect(
  CampaignRuns.Service,
  Effect.gen(function*() {
    const runs = yield* StoreRuns.Service
    return CampaignRuns.Service.of({
      captures: runs.captures,
      read: (capture, options) => Effect.map(runs.read(capture, options), (stored) => stored.map(runOf)),
      envs: runs.envs,
      keptCells: runs.keptCells
    })
  })
)

export * as BaseCampaignRuns from "./base-campaign-runs.ts"
