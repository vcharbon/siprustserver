/**
 * What a run takes of the campaign it names (`@sip/workspace` `campaigns.ts`):
 * the capture it is recorded under, the globs it stays within, the lanes it
 * takes when its line names none, the tags it allows (the campaign's and
 * `--allow`'s), whether a full run reports coverage, and its setup operations.
 */
import type { LaneCatalog } from "@sip/rig"
import type { Campaigns } from "@sip/workspace"
import type * as Arr from "effect/Array"
import type { Lane } from "./cell.js"
import { lanesIn } from "./lanes.js"
import { allowOf } from "./selection.js"

export interface CampaignRun {
  readonly name: string
  readonly cases: ReadonlyArray<string>
  /** At least one; null where the campaign states none. */
  readonly lanes: Arr.NonEmptyReadonlyArray<Lane> | null
  readonly allow: ReadonlyArray<string>
  readonly coverage: boolean
  /** The operations of the rig path it injects after provisioning and heals at its end. */
  readonly setup: ReadonlyArray<string>
}

/** A campaign as a run reads it: its defined fields and whether its full runs report coverage. */
export type Covering = Campaigns.Base & { readonly coverage: boolean }

/**
 * The run of `campaign` (the one a run named, or why it cannot be chosen),
 * its lanes read against `catalog` under the pack's lane `prefix` and its
 * allowed tags beside `allow` (`--allow`); or why not.
 */
export const campaignRunOf = (
  prefix: string,
  campaign: Covering | string,
  allow: string | undefined,
  catalog: LaneCatalog.Catalog
): CampaignRun | string => {
  if (typeof campaign === "string") return campaign
  const lanes = campaign.lanes === null ? null : lanesIn(prefix, campaign.lanes.join(","), catalog)
  if (typeof lanes === "string") return `campaign ${campaign.name}: ${lanes}`
  return {
    name: campaign.name,
    cases: campaign.cases,
    lanes,
    allow: [...new Set([...campaign.allow, ...allowOf(allow ?? "")])],
    coverage: campaign.coverage,
    setup: campaign.setup
  }
}
