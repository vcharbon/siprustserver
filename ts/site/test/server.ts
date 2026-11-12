/**
 * The whole site on an ephemeral port over the fixture pack: every site
 * service built as a deployment composes it, over the upstream deployment
 * seams' defaults, a session tree, and a store, campaign runs and campaign
 * cases that answer only the members a test states. A member no test states
 * dies naming itself: reaching one is the test's own defect.
 */
import { NodeHttpServer, NodeServices } from "@effect/platform-node"
import { TicketStates } from "@sip/lab-host"
import { Collectors, Identity, Launcher, Roots, Tools, Tracker } from "@sip/corpus"
import {
  Attachments,
  Batches,
  CallHistory,
  Calls,
  CaseDocuments,
  Cuts,
  Declared,
  Jobs,
  Population,
  Refusals,
  Removal,
  Requests,
  Resolver,
  RuleHits,
  Runs,
  RunSets,
  RunSummaries,
  Sessions,
  Sources,
  StoreBinding,
  Verifications
} from "@sip/store"
import { Effect, Layer } from "effect"
import type * as Context from "effect/Context"
import { HttpClient, HttpRouter } from "effect/unstable/http"
import { HttpApiBuilder } from "effect/unstable/httpapi"
import { Api } from "../src/api.ts"
import { AttachmentFile } from "../src/attachment-file.ts"
import { AttachmentReads } from "../src/attachments.ts"
import { Campaign } from "../src/campaign.ts"
import { CampaignCases } from "../src/campaign-cases.ts"
import { CampaignRuns } from "../src/campaign-runs.ts"
import { Catalog } from "../src/catalog.ts"
import { Collect } from "../src/collect.ts"
import { Compression } from "../src/compression.ts"
import { ViewerConfig } from "../src/config.ts"
import { Deltas } from "../src/deltas.ts"
import { Handlers } from "../src/handlers.ts"
import { Inbox } from "../src/inbox.ts"
import { Ingest } from "../src/ingest.ts"
import { JobConsole } from "../src/jobs.ts"
import { JobLaunch } from "../src/launch.ts"
import { Links } from "../src/links.ts"
import { LlmsRoute } from "../src/llms-route.ts"
import { PcapList } from "../src/pcaps.ts"
import { Reports } from "../src/reports.ts"
import { RequestBuilder } from "../src/requests.ts"
import { routerConfig } from "../src/router-config.ts"
import { RulesCatalog } from "../src/rules.ts"
import { RunText } from "../src/run-text.ts"
import { RunTrace } from "../src/run-trace.ts"
import { RunWindow } from "../src/run-window.ts"
import { ApiSchemaErrors } from "../src/schema-errors.ts"
import { ShellRoutes } from "../src/shell-routes.ts"
import { SitePack } from "../src/site-pack.ts"
import { SourceList } from "../src/sources.ts"
import { StaticRoutes } from "../src/static-routes.ts"
import { TriageCatalog } from "../src/triage.ts"
import { PACK } from "./fixture-pack.ts"

type ShapeOf<K> = K extends Context.Key<infer _I, infer S> ? S : never

/** What reaching `member`, which the test does not state, is: a defect naming it. */
const unstated = (member: string) => Effect.die(new Error(`${member} is not asked in this test`))

/**
 * `tag`'s service answering `members`; any other member, read, is a function
 * that dies naming it, except those `effects` names, which are effects that
 * do (`effects` lists the service's effect-valued members).
 */
export const stub = <I, S extends object>(
  tag: Context.Key<I, S>,
  members: Partial<S> = {},
  effects: ReadonlyArray<keyof S & string> = []
): Layer.Layer<I> =>
  Layer.succeed(
    tag,
    new Proxy(members, {
      get: (held, member) =>
        member in held
          ? (held as Record<PropertyKey, unknown>)[member]
          : typeof member === "symbol" || member === "then"
          ? undefined
          : (effects as ReadonlyArray<string>).includes(member)
          ? unstated(`${tag.key}.${member}`)
          : () => unstated(`${tag.key}.${member}`)
    }) as S
  )

/** What a test states of the store, the campaign runs and the campaign cases: per service, the members it answers. */
export interface Members {
  readonly attachments?: Partial<ShapeOf<typeof Attachments.Service>>
  readonly batches?: Partial<ShapeOf<typeof Batches.Service>>
  readonly callHistory?: Partial<ShapeOf<typeof CallHistory.Service>>
  readonly calls?: Partial<ShapeOf<typeof Calls.Service>>
  readonly campaignCases?: Partial<ShapeOf<typeof CampaignCases.Service>>
  readonly campaignRuns?: Partial<ShapeOf<typeof CampaignRuns.Service>>
  readonly caseDocuments?: Partial<ShapeOf<typeof CaseDocuments.Service>>
  readonly cuts?: Partial<ShapeOf<typeof Cuts.Service>>
  readonly declared?: Partial<ShapeOf<typeof Declared.Service>>
  readonly jobs?: Partial<ShapeOf<typeof Jobs.Service>>
  readonly population?: Partial<ShapeOf<typeof Population.Service>>
  readonly refusals?: Partial<ShapeOf<typeof Refusals.Service>>
  readonly removal?: Partial<ShapeOf<typeof Removal.Service>>
  readonly requests?: Partial<ShapeOf<typeof Requests.Service>>
  readonly resolver?: Partial<ShapeOf<typeof Resolver.Service>>
  readonly ruleHits?: Partial<ShapeOf<typeof RuleHits.Service>>
  readonly runs?: Partial<ShapeOf<typeof Runs.Service>>
  readonly runSets?: Partial<ShapeOf<typeof RunSets.Service>>
  readonly runSummaries?: Partial<ShapeOf<typeof RunSummaries.Service>>
  readonly sources?: Partial<ShapeOf<typeof Sources.Service>>
  readonly verifications?: Partial<ShapeOf<typeof Verifications.Service>>
  readonly runWindow?: Partial<ShapeOf<typeof RunWindow.Service>>
}

/** The store, the campaign runs and the campaign cases answering what `members` states (a run's window among them), beside the store's own binding and no ticket. */
const storeOf = (members: Members) =>
  Layer.mergeAll(
    stub(Attachments.Service, members.attachments),
    stub(Batches.Service, members.batches),
    stub(CallHistory.Service, members.callHistory),
    stub(Calls.Service, members.calls),
    stub(CampaignCases.Service, members.campaignCases),
    stub(CampaignRuns.Service, members.campaignRuns, ["captures"]),
    stub(CaseDocuments.Service, members.caseDocuments),
    stub(Cuts.Service, members.cuts, ["standingPrep", "latest"]),
    stub(Declared.Service, members.declared, ["declared", "population"]),
    stub(Jobs.Service, members.jobs, ["running"]),
    stub(Population.Service, members.population, ["list", "captures", "size"]),
    stub(Refusals.Service, members.refusals, ["standing"]),
    stub(Removal.Service, members.removal),
    stub(Requests.Service, members.requests),
    stub(Resolver.Service, members.resolver),
    stub(RuleHits.Service, members.ruleHits),
    stub(Runs.Service, members.runs, ["lanes", "protectedBundles", "allBundles", "unfinished"]),
    stub(RunSets.Service, members.runSets),
    stub(RunSummaries.Service, members.runSummaries),
    stub(Sources.Service, members.sources),
    stub(Verifications.Service, members.verifications),
    stub(RunWindow.Service, members.runWindow),
    StoreBinding.layer,
    TicketStates.layerWith(() => Effect.succeed(new Map()))
  )

/** No tool is built; no file decides an extraction or a cut, so their fingerprints are of nothing. */
const NoTools = Tools.layerWith({
  ready: (tool) => Effect.fail(new Tools.ToolUnavailable({ tool, detail: "no tool is built here" })),
  generator: Effect.fail(new Tools.ToolUnavailable({ tool: "generator", detail: "no tool is built here" })),
  measure: Effect.fail(new Tools.ToolUnavailable({ tool: "measure", detail: "no tool is built here" })),
  compileMs: Effect.succeed(0),
  decides: Effect.succeed({ root: "/nowhere", extraction: [], cut: [] }),
  reading: []
})

/** The roots under nowhere, a source pcap's under `/nowhere/pcaps` besides the uploads. */
const NowhereRoots = Roots.layerWith({
  paths: (overrides) =>
    Effect.succeed({
      corpus: overrides.corpus ?? "/nowhere/corpus",
      sessions: overrides.sessions ?? "/nowhere/sessions",
      census: overrides.census ?? "/nowhere/census",
      registry: overrides.registry ?? "/nowhere/registry.json",
      repoRoot: overrides.repoRoot ?? "/nowhere"
    }),
  pcapRoots: Effect.succeed(["/nowhere/pcaps"]),
  locks: Effect.succeed("/nowhere/locks"),
  bench: Effect.succeed("/nowhere/bench")
})

/** The upstream deployment seams as they stand before any deployment substitutes its own, the tools and roots apart. */
const Deployment = Layer.mergeAll(
  Collectors.layer,
  Launcher.layer,
  Identity.layer,
  Tracker.layer,
  NoTools
).pipe(Layer.provideMerge(NowhereRoots))

/** Every site service, as a deployment's composition builds it. */
const Site = Layer.provideMerge(
  Links.layer,
  Layer.mergeAll(
    Catalog.layer,
    RunText.layer.pipe(Layer.provide(Layer.mergeAll(TriageCatalog.layer, RunTrace.layer))),
    RunTrace.layer,
    TriageCatalog.layer,
    Deltas.layer,
    RulesCatalog.layer,
    Ingest.layer,
    RequestBuilder.layer,
    JobLaunch.layer,
    JobConsole.layer,
    SourceList.layer,
    PcapList.layer,
    Inbox.scanner,
    Reports.layer,
    Campaign.layer,
    AttachmentReads.layer,
    Collect.layer
  )
).pipe(Layer.provideMerge(Inbox.layer))

/** The site's routes over `config`, the store answering `members` and the session tree at `sessions`, and a client of it. */
export const site = (config: ViewerConfig.Values, members: Members = {}, sessions = "/nowhere/sessions") =>
  HttpRouter.serve(
    Layer.mergeAll(
      HttpApiBuilder.layer(Api, { openapiPath: "/api/openapi.json" }).pipe(
        Layer.provide(Handlers),
        Layer.provide(ApiSchemaErrors.layer)
      ),
      AttachmentFile.layer,
      LlmsRoute,
      ShellRoutes,
      StaticRoutes,
      Compression
    ),
    { routerConfig }
  ).pipe(
    Layer.provide(Site),
    Layer.provide(Deployment),
    Layer.provide(storeOf(members)),
    Layer.provide(Sessions.layer(sessions)),
    Layer.provide(Layer.succeed(ViewerConfig.Service, config)),
    Layer.provide(Layer.succeed(SitePack.Service, PACK)),
    Layer.provideMerge(NodeHttpServer.layerTest),
    Layer.provide(NodeServices.layer)
  )

/** A request's status and body, the body parsed where it is JSON. */
export interface Answer {
  readonly status: number
  readonly body: string
  readonly json: unknown
}

/** `request` sent to the site over `config` and `members`, answered. */
export const ask = (
  config: ViewerConfig.Values,
  members: Members,
  request: (client: HttpClient.HttpClient) => Effect.Effect<{ readonly status: number; readonly text: Effect.Effect<string, unknown> }, unknown>,
  sessions?: string
): Promise<Answer> =>
  Effect.runPromise(
    Effect.gen(function*() {
      const response = yield* request(yield* HttpClient.HttpClient)
      const body = yield* response.text
      return { status: response.status, body, json: parsed(body) }
    }).pipe(Effect.scoped, Effect.provide(site(config, members, sessions)))
  )

const parsed = (body: string): unknown => {
  try {
    return JSON.parse(body)
  } catch {
    return undefined
  }
}

/** `GET path` on the site. */
export const get = (config: ViewerConfig.Values, members: Members, path: string, sessions?: string): Promise<Answer> =>
  ask(config, members, (client) => client.get(path), sessions)
