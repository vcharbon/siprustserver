/**
 * `siprig-site`, the base image's site for a pack that ships none: the
 * workspace `SIPRIG_WORKSPACE` names applied to the environment (its site
 * port, bind address and public URL), then the whole `@sip/site` over the
 * base's site pack (`base-site-pack.ts`), titled by the
 * pack manifest `SIPRIG_PACK` holds ("siprig" where it names none), its rig
 * path the pack's then the workspace's, the store `DATABASE_URL` names under
 * the store's own binding, and the upstream deployment seams' defaults. A
 * refused manifest, an unreachable store or a pending migration fails the
 * server outright.
 */
import { NodeHttpServer, NodeRuntime, NodeServices } from "@effect/platform-node"
import { Roots, TicketStates } from "@sip/lab-host"
import { Collectors, Identity, Launcher, Tools, Tracker } from "@sip/corpus"
import { LaneCatalog, RigPath } from "@sip/rig"
import * as Pack from "@sip/rig/pack"
import {
  Attachments,
  Batches,
  CallHistory,
  Calls,
  CampaignRuns,
  CaseDocuments,
  Client,
  Cuts,
  Declared,
  Jobs,
  Population,
  Refusals,
  Removal,
  Requests,
  RuleHits,
  Runs,
  RunSets,
  RunSummaries,
  SchemaState,
  Sessions,
  Sources,
  StoreBinding,
  Verifications
} from "@sip/store"
import { Api } from "@sip/site/api"
import { AttachmentFile } from "@sip/site/attachment-file"
import { AttachmentReads } from "@sip/site/attachments"
import { Campaign } from "@sip/site/campaign"
import { CampaignCases } from "@sip/site/campaign-cases"
import { Catalog } from "@sip/site/catalog"
import { Collect } from "@sip/site/collect"
import { Compression } from "@sip/site/compression"
import { ViewerConfig } from "@sip/site/config"
import { Deltas } from "@sip/site/deltas"
import { Handlers } from "@sip/site/handlers"
import { Inbox } from "@sip/site/inbox"
import { Ingest } from "@sip/site/ingest"
import { JobConsole } from "@sip/site/jobs"
import { JobLaunch } from "@sip/site/launch"
import { Links } from "@sip/site/links"
import { LlmsRoute } from "@sip/site/llms-route"
import { PcapList } from "@sip/site/pcaps"
import { Reports } from "@sip/site/reports"
import { RequestBuilder } from "@sip/site/requests"
import { routerConfig } from "@sip/site/router-config"
import { RulesCatalog } from "@sip/site/rules"
import { RunText } from "@sip/site/run-text"
import { RunTrace } from "@sip/site/run-trace"
import { RunWindow } from "@sip/site/run-window"
import { ApiSchemaErrors } from "@sip/site/schema-errors"
import { ShellRoutes } from "@sip/site/shell-routes"
import { SitePack } from "@sip/site/site-pack"
import { SourceList } from "@sip/site/sources"
import { StaticRoutes } from "@sip/site/static-routes"
import { TriageCatalog } from "@sip/site/triage"
import { Effect, Layer, Result } from "effect"
import * as FileSystem from "effect/FileSystem"
import { HttpRouter } from "effect/unstable/http"
import { HttpApiBuilder } from "effect/unstable/httpapi"
import { createServer } from "node:http"
import * as path from "node:path"
import { BaseCampaignRuns } from "./base-campaign-runs.ts"
import { LAB, packOf } from "./base-site-pack.ts"
import { apply as applyWorkspace, of as workspaceOf, packDirOf } from "./base-workspace.ts"

const ApiRoutes = HttpApiBuilder.layer(Api, { openapiPath: "/api/openapi.json" }).pipe(
  Layer.provide(Handlers),
  Layer.provide(ApiSchemaErrors.layer)
)

const ServerLayer = Layer.unwrap(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    yield* Effect.logInfo(`site on ${config.publicUrl} (bound to ${config.bind} port ${config.port}) · config ${config.configPath}`)
    return HttpRouter.serve(Layer.mergeAll(ApiRoutes, AttachmentFile.layer, LlmsRoute, ShellRoutes, StaticRoutes, Compression), {
      routerConfig
    }).pipe(Layer.provide(NodeHttpServer.layer(createServer, { port: config.port, host: config.bind })))
  })
)

/** The store the config names, else `DATABASE_URL`'s, guarded against a pending migration of the store's own binding. */
const DatabaseLayer = Layer.unwrap(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const url = config.triage?.database
    return SchemaState.guarded(url === undefined ? Client.layer : Client.layerFromUrl(url)).pipe(Layer.provide(StoreBinding.layer))
  })
)

/** The session tree the run rows name their bundles under: the config's, else its own directory, never opened. */
const SessionsLayer = Layer.unwrap(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    return Sessions.layer(config.triage?.sessions ?? path.dirname(config.configPath))
  })
)

const StoreLayer = Layer.provideMerge(
  Requests.layer,
  Layer.mergeAll(
    TicketStates.layerUnavailable("this pack states no tracker"),
    StoreBinding.resolver,
    RunSets.layer,
    RunSummaries.layer,
    CaseDocuments.layer,
    Batches.layer,
    Jobs.layer,
    Sources.layer,
    Removal.layer,
    Population.layer,
    Runs.layer,
    Verifications.layer,
    Attachments.layer,
    CampaignRuns.layer,
    Calls.layer,
    CallHistory.layer,
    Refusals.layer,
    RuleHits.layer,
    Declared.layer,
    Cuts.layer
  )
).pipe(Layer.provideMerge(StoreBinding.layer))

const ViewerLayer = Layer.provideMerge(Links.layer, Layer.mergeAll(
  Catalog.layer,
  RunText.layer.pipe(Layer.provide(Layer.mergeAll(TriageCatalog.layer, RunTrace.layer, RunWindow.layer))),
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
  CampaignCases.layerOf(() => Effect.succeed(undefined)),
  AttachmentReads.layer,
  Collect.layer
)).pipe(Layer.provideMerge(Inbox.layer))

const DeploymentLayer = Layer.mergeAll(Collectors.layer, Launcher.layer, Identity.layer, Tracker.layer, Tools.layer).pipe(
  Layer.provideMerge(Roots.layer(LAB))
)

/** The pack's title, the manifest at `dir` holds; "siprig" where no pack is named. */
const titleOf = (dir: string | undefined) =>
  Effect.gen(function*() {
    if (dir === undefined) return "siprig"
    const file = path.join(dir, Pack.FILE)
    const text = yield* (yield* FileSystem.FileSystem).readFileString(file)
    return yield* Result.match(Pack.parse(file, text), { onSuccess: (pack) => Effect.succeed(pack.title), onFailure: Effect.fail })
  })

/** The base's site pack over the pack's then the workspace's rig path, with no lane of its own. */
const PackLayer = Layer.effect(
  SitePack.Service,
  Effect.gen(function*() {
    const pack = packDirOf(process.env)
    const workspace = workspaceOf(process.env)?.root
    const rig = yield* RigPath.load({ pack, workspace })
    const lanes: LaneCatalog.Catalog = { dirs: LaneCatalog.dirsUnder(workspace ?? LAB.checkout), rig, lanes: [] }
    return packOf(yield* titleOf(pack), lanes)
  })
)

const MainLayer = ServerLayer.pipe(
  Layer.provide(ViewerLayer),
  Layer.provide(DeploymentLayer),
  Layer.provide(BaseCampaignRuns.layer),
  Layer.provide(StoreLayer),
  Layer.provide(DatabaseLayer),
  Layer.provide(SessionsLayer),
  Layer.provideMerge(ViewerConfig.layer),
  Layer.provideMerge(PackLayer),
  Layer.provide(NodeServices.layer)
)

applyWorkspace(process.env).pipe(
  Effect.andThen(Layer.launch(MainLayer)),
  Effect.provide(NodeServices.layer),
  NodeRuntime.runMain
)
