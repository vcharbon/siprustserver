/**
 * The case tags a campaign start offers: those a lane picked excludes, each
 * with the cases carrying it, and what a start then asks for. The names, labels
 * and warnings are the server's; the page holds no rule of its own.
 */
import type { CampaignCase, CampaignRun, LaunchTag } from "./api.ts"

/** A tag a start offers, with how many cases carry it. */
export interface OfferedTag extends LaunchTag {
  readonly cases: number
}

/** The tags a lane of `picked` excludes, each counted over `cases`. */
export const offered = (
  tags: readonly LaunchTag[],
  picked: readonly string[],
  cases: readonly CampaignCase[]
): readonly OfferedTag[] =>
  tags
    .filter((tag) => tag.excludedOn.some((lane) => picked.includes(lane)))
    .map((tag) => ({ ...tag, cases: cases.filter((one) => one.tags.includes(tag.tag)).length }))

/** What a start asks for: the tags allowed that the picked lanes still exclude. */
export const asked = (allowed: readonly string[], offers: readonly OfferedTag[]): readonly string[] =>
  offers.map((offer) => offer.tag).filter((tag) => allowed.includes(tag))

/** Why the newest whole-campaign run left `caseId` out, one line per lane; none where it ran it or no such run is read. */
export const exclusionsOf = (
  runs: readonly CampaignRun[],
  caseId: string
): readonly { readonly lane: string; readonly reason: string }[] => {
  const whole = runs.filter((run) => run.scope === null)
  const newest = whole.reduce<CampaignRun | undefined>((best, run) => (best === undefined || run.id > best.id ? run : best), undefined)
  return (newest?.lanes ?? []).flatMap((lane) =>
    lane.excluded.filter((one) => one.caseId === caseId).map((one) => ({ lane: lane.lane, reason: one.reason }))
  )
}

/** The declared tags `carried` names, in the order the case states them. */
export const carriedBy = (tags: readonly LaunchTag[], carried: readonly string[]): readonly LaunchTag[] =>
  carried.flatMap((name) => tags.filter((tag) => tag.tag === name))
