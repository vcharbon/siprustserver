/**
 * The PLATFORM PROFILE: the constant facts of how the replaying platform
 * handles a call, stated as data where the case policy reads them.
 *
 * {@link NEUTRAL_PLATFORM} states nothing: no in-dialog INVITE is relayed, no
 * actor answers an audit. A fact a profile leaves false derives nothing, so the
 * run shows what the platform does instead of what a guess said it would.
 */
import type { Placement } from "@sip/contracts"
import type { BackgroundMap } from "./background.js"
import type { Layout } from "./topology.js"

export interface PlatformProfile {
  /**
   * Whether the platform relays an in-dialog INVITE end to end. Where it does,
   * a leg the vantage lost past the 2xx its peer sent is owed the far side of
   * every such exchange the other leg holds, and synthesis transcribes it there
   * (`far-side-reinvite.ts`).
   */
  readonly relaysReinvite: boolean
  /**
   * Whether the platform audits every dialog it holds with an in-dialog
   * `OPTIONS`, which every actor owning a leg answers `200` (§5.1). The policy
   * also collapses the captured side's own locally-minted `OPTIONS`
   * exchanges into it (`background.ts`); an `OPTIONS` relayed end to end keeps
   * its steps. The policy states no count: the cadence is the platform's timer.
   */
  readonly answersOptionsAudit: boolean
}

/** The profile of a platform no deployment has described. */
export const NEUTRAL_PLATFORM: PlatformProfile = {
  relaysReinvite: false,
  answersOptionsAudit: false
}

/** The `OPTIONS` audit an actor answers `200`. */
export const OPTIONS_AUDIT: Placement.BackgroundPolicy = {
  match: { method: "OPTIONS" },
  respond: { status: 200 }
}

/** The background policies each actor of `layout` states under `profile`. */
export const backgroundOf = (profile: PlatformProfile, layout: Layout): BackgroundMap =>
  profile.answersOptionsAudit ? new Map(layout.legs.map((l) => [l.actor, [OPTIONS_AUDIT]])) : new Map()
