/**
 * The overview of any page URL (`GET /api/links?url=`): the route table's base
 * overview, made richer where this server's data states more — a campaign
 * page, a triage study — and closed by the site map and the OpenAPI
 * document. A URL that names no page is refused, saying why.
 */
import { CallHistory, Runs, StoreBinding } from "@sip/store"
import {
  absolute,
  baseOverview,
  LLMS_PATH,
  OPENAPI_PATH,
  type Overview,
  type OverviewSection,
  pageUrlOf,
  routeOf
} from "@sip/viewer-core/routes"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { OpenApi } from "effect/unstable/httpapi"
import { Api, LinksRefused, type PageMissing } from "./api.ts"
import { AttachmentReads } from "./attachments.ts"
import { Campaign } from "./campaign.ts"
import { CampaignCases } from "./campaign-cases.ts"
import { ViewerConfig } from "./config.ts"
import { campaignOverview } from "./links-campaign.ts"
import { sized } from "./links-cell.ts"
import { captureOverview } from "./links-capture.ts"
import { studyOverview } from "./links-study.ts"
import { RunText } from "./run-text.ts"
import { RunTrace } from "./run-trace.ts"
import type { SitePack } from "./site-pack.ts"
import { TriageCatalog } from "./triage.ts"

export interface Interface {
  /** The overview of `url` (as pasted: absolute or site-relative), its links absolute under `origin`. */
  readonly overview: (url: string, origin: string) => Effect.Effect<Overview, LinksRefused | PageMissing>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Links") {}

const siteSection = (origin: string, openapi: number): OverviewSection => ({
  heading: "Site",
  lines: [],
  links: [
    { label: "every page and how to reach it", href: absolute(origin, LLMS_PATH) },
    { label: sized("OpenAPI, one description per endpoint", openapi), href: absolute(origin, OPENAPI_PATH) }
  ]
})

type Needs =
  | SitePack.Service
  | Campaign.Service
  | AttachmentReads.Service
  | CampaignCases.Service
  | ViewerConfig.Service
  | RunTrace.Service
  | RunText.Service
  | TriageCatalog.Service
  | CallHistory.Service
  | StoreBinding.Service
  | Runs.Service

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const context = yield* Effect.context<Needs>()
    const openapi = JSON.stringify(OpenApi.fromApi(Api)).length

    const overview = Effect.fn("Links.overview")(function*(raw: string, origin: string) {
      const url = pageUrlOf(raw, origin)
      const read = routeOf(url, raw)
      if (!("route" in read) || url === undefined) {
        const reason = "reason" in read ? read.reason : `not a URL: ${raw}`
        return yield* new LinksRefused({ reason, llms: absolute(origin, LLMS_PATH) })
      }
      const { route } = read
      const base = baseOverview(route, url, origin)
      const rich: Overview = route.kind === "campaign"
        ? yield* campaignOverview(base, route.capture, url.searchParams, origin)
        : route.kind === "triage-study"
        ? url.searchParams.get("case") === null
          ? yield* captureOverview(base, route.capture, origin)
          : yield* studyOverview(base, route.capture, url.searchParams.get("case")!, url.searchParams, origin)
        : base
      return { ...rich, sections: [...rich.sections, siteSection(origin, openapi)] } satisfies Overview
    }, Effect.provideContext(context))

    return Service.of({ overview })
  })
)

export * as Links from "./links.ts"
