/** Handlers: decode, ask the service, map its one failure to its status. Nothing else. */
import { IdForms } from "@sip/corpus"
import { CallHistory } from "@sip/store"
import { overviewToMarkdown, refusalToMarkdown } from "@sip/viewer-core/routes"
import { Effect, Layer } from "effect"
import { HttpApiBuilder } from "effect/unstable/httpapi"
import { Api, CaptureRefused, JobRefused, NotFound, ReportRefused, ReportUnavailable, RequestRefused } from "./api.ts"
import { AttachmentReads } from "./attachments.ts"
import { Collect } from "./collect.ts"
import { HOLDER_FORMS, holderOf } from "./attachment-holder.ts"
import { Campaign } from "./campaign.ts"
import { CampaignCases } from "./campaign-cases.ts"
import { Catalog } from "./catalog.ts"
import { ViewerConfig } from "./config.ts"
import { Deltas } from "./deltas.ts"
import { Ingest } from "./ingest.ts"
import { DEFAULT_CELLS, DEFAULT_LIMIT as DEFAULT_JOBS, JobConsole } from "./jobs.ts"
import { DEFAULT_KNOBS, lanes } from "./lanes.ts"
import { JobLaunch } from "./launch.ts"
import { Links } from "./links.ts"
import { PcapList } from "./pcaps.ts"
import { Reports } from "./reports.ts"
import { RequestBuilder } from "./requests.ts"
import { RulesCatalog } from "./rules.ts"
import { RunText } from "./run-text.ts"
import { RunTrace } from "./run-trace.ts"
import { readDigest } from "./cell-digest.ts"
import { digestToMarkdown } from "./cell-digest-text.ts"
import { negotiated } from "./negotiate.ts"
import { originOf } from "./origin.ts"
import { messageText } from "./cell-message.ts"
import { trimLegs } from "./flows-trim.ts"
import { SitePack } from "./site-pack.ts"
import { siteName } from "./site-name.ts"
import { DEFAULT_LIMIT, SourceList } from "./sources.ts"
import { TriageCatalog } from "./triage.ts"

/** A service's 404 as the wire states it, with the form the route takes its id in. */
const notFound = (
  error: { readonly id: string; readonly accepted?: string | undefined; readonly note?: string | undefined }
): NotFound =>
  new NotFound({
    id: error.id,
    ...(error.accepted === undefined ? {} : { accepted: error.accepted }),
    ...(error.note === undefined ? {} : { note: error.note })
  })

export const CatalogHandlers = HttpApiBuilder.group(
  Api,
  "catalog",
  Effect.fn(function*(handlers) {
    const catalog = yield* Catalog.Service
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const laneCatalog = pack.lanes

    return handlers.handleAll({
      config: () =>
        Effect.succeed({
          name: siteName(pack.title),
          configPath: config.configPath,
          caseRoots: config.caseRoots,
          runRoots: config.runRoots,
          flowsRoots: config.flowsRoots,
          triage: config.triage,
          port: config.port,
          bind: config.bind,
          publicUrl: config.publicUrl,
          staticDir: config.staticDir,
          commands: config.commands
        }),
      lanes: () => Effect.succeed({ lanes: lanes(laneCatalog), knobs: DEFAULT_KNOBS }),
      cases: () => catalog.listCases,
      case: ({ params, query }) =>
        catalog.readCase(params.id, query.variant).pipe(
          Effect.mapError(notFound)
        ),
      caseSource: ({ params, query }) =>
        catalog.readCaseSource(params.id, query.variant).pipe(
          Effect.mapError(notFound)
        ),
      caseSourceLegs: ({ params, query }) =>
        catalog.readCaseSourceLegs(params.id, query.variant, query.from ?? 0, query.count).pipe(
          Effect.mapError(notFound)
        ),
      runs: () => catalog.listRuns,
      run: ({ params }) =>
        catalog.readRun(params.id).pipe(
          Effect.mapError(notFound)
        )
    })
  })
)

/** The runs a call study lists: enough to see the recent history of every lane. */
const CALL_HISTORY_LIMIT = 10


export const TriageHandlers = HttpApiBuilder.group(
  Api,
  "triage",
  Effect.fn(function*(handlers) {
    const triage = yield* TriageCatalog.Service
    const runText = yield* RunText.Service
    const traces = yield* RunTrace.Service
    const history = yield* CallHistory.Service
    const reports = yield* Reports.Service
    const forms = yield* IdForms.caseFormsOf
    const pack = yield* SitePack.Service
    const laneCatalog = pack.lanes
    const digestReads = yield* Effect.context<TriageCatalog.Service | SitePack.Service>()
    return handlers.handleAll({
      triageQueue: ({ query }) => triage.queue(query),
      triageCallHistory: ({ params, query }) =>
        history.ofCall(params.capture, query.case, lanes(laneCatalog).map((one) => one.lane), query.limit ?? CALL_HISTORY_LIMIT)
          .pipe(Effect.orDie),
      triageStudy: ({ params, query }) =>
        triage.study(
          params.capture,
          query.lane,
          query.case === undefined ? undefined : forms.caseIdIn(params.capture, query.case)
        ).pipe(
          Effect.mapError(notFound)
        ),
      triageFlowsEnvelope: ({ params }) =>
        triage.flowsEnvelope(params.capture).pipe(
          Effect.mapError(notFound)
        ),
      triageFlowsLegs: ({ params, query }) =>
        triage.flowsLegs(params.capture, query.from ?? 0, query.count).pipe(
          Effect.map((window) => trimLegs(window, query.raw ?? "full")),
          Effect.mapError(notFound)
        ),
      triageCellDigest: ({ params, query, request }) =>
        readDigest(params.capture, params.run, params.cell, query.lane, originOf(request)).pipe(
          Effect.provideContext(digestReads),
          Effect.map((digest) => negotiated(request, digest, digestToMarkdown)),
          Effect.mapError(notFound)
        ),
      triageCellMessage: ({ params, query }) =>
        triage.cell(params.capture, params.run, params.cell).pipe(
          Effect.flatMap((detail) =>
            messageText(
              detail,
              { m: query.m, step: query.step },
              (from) => triage.flowsLegs(params.capture, from, 1)
            )
          ),
          Effect.mapError(notFound)
        ),
      triageCaseDocument: ({ params, query }) => {
        const caseId = forms.caseIdIn(params.capture, query.case)
        return triage.study(params.capture, undefined, caseId).pipe(
          Effect.flatMap((study) => {
            const one = study.cases.find((entry) => entry.id === caseId)
            return one === undefined || one.document === null
              ? Effect.fail(new NotFound({ id: `${params.capture}/${caseId}`, note: "no cut document of this call" }))
              : Effect.succeed(one.document)
          }),
          Effect.mapError(notFound)
        )
      },
      triageCell: ({ params }) =>
        triage.cell(params.capture, params.session, params.cell).pipe(
          Effect.mapError(notFound)
        ),
      // A run whose bundle is gone is read as the store still holds it: its row,
      // or the summary a citation kept once the wipe dropped the row.
      // The run and the case are read in every form a ticket cites them in
      // (`1234`, `run1234`, `@run:1234`; `case110`, `-case110`, the full id).
      triageRunText: ({ params, query }) => {
        const id = IdForms.numbered("run", params.run)
        const run = id === undefined ? params.run : `run${id}`
        const caseId = query.case === undefined ? undefined : forms.caseIdIn(params.capture, query.case)
        return runText.text(params.capture, run, query.lane, query.full === "1", caseId, query.cell).pipe(
          // A cell is read off the run's bundle alone: a run whose bundle is
          // gone has no cell to read, so its kept summary never answers for one.
          Effect.catchTag("Site.NotFound", (missing) => {
            return id === undefined || query.cell !== undefined
              ? Effect.fail(missing)
              : reports.runText(params.capture, id, query.lane, caseId).pipe(
                Effect.flatMap((answer) =>
                  answer._tag === "text"
                    ? Effect.succeed(answer.text)
                    : Effect.fail(new NotFound({ id: missing.id, accepted: pack.triage.forms.run, note: answer.note }))
                )
              )
          }),
          Effect.mapError(notFound)
        )
      },
      triageCellTraces: ({ params }) =>
        traces.list(params.capture, params.run, params.cell).pipe(
          Effect.mapError(notFound)
        ),
      triageCellTrace: ({ params, query }) =>
        traces.read(params.capture, params.run, params.cell, query.file).pipe(
          Effect.mapError(notFound)
        ),
    })
  })
)

export const DeltaHandlers = HttpApiBuilder.group(
  Api,
  "deltas",
  Effect.fn(function*(handlers) {
    const deltas = yield* Deltas.Service
    return handlers.handleAll({
      deltaSheet: ({ query }) => deltas.sheet(query)
    })
  })
)

export const CaptureHandlers = HttpApiBuilder.group(
  Api,
  "captures",
  Effect.fn(function*(handlers) {
    const ingest = yield* Ingest.Service
    return handlers.handleAll({
      captureIngest: ({ payload }) =>
        ingest.accept({
          files: payload.file.map((one) => ({ filename: one.name, path: one.path })),
          lane: payload.lane
        }).pipe(
          Effect.mapError((error) =>
            new CaptureRefused({ filename: error.filename, detail: error.detail })
          )
        ),
      captureIngestState: ({ params }) => ingest.state(params.capture)
    })
  })
)

export const RulesHandlers = HttpApiBuilder.group(
  Api,
  "rules",
  Effect.fn(function*(handlers) {
    const rules = yield* RulesCatalog.Service
    return handlers.handleAll({ rules: ({ query }) => rules.view(query.lane) })
  })
)

export const RequestHandlers = HttpApiBuilder.group(
  Api,
  "requests",
  Effect.fn(function*(handlers) {
    const builder = yield* RequestBuilder.Service
    return handlers.handleAll({
      requestPreview: ({ payload }) =>
        builder.preview(payload).pipe(Effect.mapError((error) => new RequestRefused({ detail: error.detail }))),
      requestCreate: ({ payload }) =>
        builder.create(payload).pipe(
          Effect.map((created) => ({ created })),
          Effect.mapError((error) => new RequestRefused({ detail: error.detail }))
        )
    })
  })
)

/** The status filter as the query spells it; anything else lists every row. */
const statusOf = (raw: string | undefined): JobConsole.ListQuery["status"] =>
  raw === "active" || raw === "ended" || raw === "queued" || raw === "running" || raw === "done" || raw === "failed" ||
    raw === "interrupted" || raw === "died"
    ? raw
    : undefined

export const JobHandlers = HttpApiBuilder.group(
  Api,
  "jobs",
  Effect.fn(function*(handlers) {
    const launch = yield* JobLaunch.Service
    const console = yield* JobConsole.Service
    const notFound = (error: JobConsole.Missing) => new NotFound({ id: `job ${error.id}` })
    const missing = (error: JobConsole.Missing | JobConsole.Refused) =>
      error._tag === "Site.JobMissing" ? notFound(error) : new JobRefused({ detail: error.detail })
    return handlers.handleAll({
      jobStart: ({ payload }) =>
        launch.start(payload).pipe(
          Effect.map((jobs) => ({ jobs })),
          Effect.mapError((error) => new JobRefused({ detail: error.detail }))
        ),
      jobList: ({ query }) =>
        console.list({ status: statusOf(query.status), limit: query.limit ?? DEFAULT_JOBS, offset: query.offset ?? 0 }),
      jobShow: ({ params }) =>
        console.show(params.id).pipe(
          Effect.map((detail) => ({
            ...detail.job,
            label: detail.label,
            mode: detail.mode,
            progress: detail.progress,
            request: detail.request,
            lineage: detail.lineage,
            lines: detail.lines,
            batchList: detail.batchList
          })),
          Effect.mapError(notFound)
        ),
      jobLog: ({ params, query }) => console.log(params.id, query.from ?? 0).pipe(Effect.mapError(notFound)),
      jobCells: ({ params, query }) =>
        console.cells(params.id, query.outcome, query.limit ?? DEFAULT_CELLS).pipe(Effect.mapError(notFound)),
      jobStop: ({ params, payload }) =>
        console.stop(params.id, payload.grace).pipe(Effect.map(() => ({ stopped: params.id })), Effect.mapError(missing)),
      jobRestart: ({ params, payload }) => console.restart(params.id, payload.mode).pipe(Effect.mapError(missing)),
      jobDeletePlan: ({ params }) => console.deletePlan(params.id).pipe(Effect.mapError(notFound)),
      jobDelete: ({ params }) => console.delete(params.id).pipe(Effect.mapError(missing))
    })
  })
)

export const SourceHandlers = HttpApiBuilder.group(
  Api,
  "sources",
  Effect.fn(function*(handlers) {
    const sources = yield* SourceList.Service
    return handlers.handleAll({
      sourceList: ({ query }) =>
        sources.list({
          stale: query.stale === "1" || query.stale === "true",
          q: query.q,
          limit: query.limit ?? DEFAULT_LIMIT,
          offset: query.offset ?? 0
        }),
      retirePlan: ({ params }) => sources.retirePlan(params.capture)
    })
  })
)

export const PcapHandlers = HttpApiBuilder.group(
  Api,
  "pcaps",
  Effect.fn(function*(handlers) {
    const pcaps = yield* PcapList.Service
    return handlers.handleAll({ pcapList: () => pcaps.list })
  })
)

export const ReportHandlers = HttpApiBuilder.group(
  Api,
  "reports",
  Effect.fn(function*(handlers) {
    const reports = yield* Reports.Service
    const refused = (error: Reports.Refused | Reports.Unavailable) =>
      error._tag === "Site.ReportRefused"
        ? new ReportRefused({ reason: error.reason })
        : new ReportUnavailable({ reason: error.reason })
    return handlers.handleAll({
      compareReport: ({ query }) =>
        reports.compare({
          ...query,
          all: query.all === "1" || query.all === "true",
          ladderChanged: query.ladderChanged === "1" || query.ladderChanged === "true"
        }).pipe(Effect.mapError(refused)),
      runReport: ({ query }) => reports.report(query).pipe(Effect.mapError(refused))
    })
  })
)

export const CampaignHandlers = HttpApiBuilder.group(
  Api,
  "campaign",
  Effect.fn(function*(handlers) {
    const campaign = yield* Campaign.Service
    const cases = yield* CampaignCases.Service
    return handlers.handleAll({
      campaignList: () => campaign.listed,
      campaignView: ({ params, query }) => campaign.read(params.capture, query),
      campaignCaseSource: ({ query }) =>
        cases.read(query.case).pipe(Effect.map((one) => one.text), Effect.mapError(notFound)),
      campaignCasePivot: ({ query }) =>
        cases.read(query.case).pipe(Effect.map((one) => one.pivot), Effect.mapError(notFound))
    })
  })
)

export const AttachmentHandlers = HttpApiBuilder.group(
  Api,
  "attachments",
  Effect.fn(function*(handlers) {
    const reads = yield* AttachmentReads.Service
    const collect = yield* Collect.Service
    return handlers.handleAll({
      attachmentList: ({ query }) => {
        const holder = holderOf(query)
        return holder === undefined
          ? Effect.fail(new NotFound({ id: "attachments", accepted: HOLDER_FORMS }))
          : reads.list(holder)
      },
      attachmentView: ({ params }) => reads.view(params.id).pipe(Effect.mapError(notFound)),
      collectOffered: ({ query }) => collect.offered(query.lane),
      collectRun: ({ payload }) => collect.run(payload)
    })
  })
)

/** The overview of a page URL: Markdown unless the request accepts JSON, a refusal in the same form. */
export const LinkHandlers = HttpApiBuilder.group(
  Api,
  "links",
  Effect.fn(function*(handlers) {
    const links = yield* Links.Service
    return handlers.handle("links", ({ query, request }) => {
      const origin = originOf(request)
      return links.overview(query.url, origin).pipe(
        Effect.map((overview) => negotiated(request, overview, overviewToMarkdown)),
        Effect.catch((refused) =>
          Effect.succeed(
            negotiated(
              request,
              refused,
              (one) => refusalToMarkdown(one, origin),
              refused._tag === "Site.PageMissing" ? 404 : 400
            )
          )
        )
      )
    })
  })
)

export const Handlers = Layer.mergeAll(
  CatalogHandlers,
  TriageHandlers,
  DeltaHandlers,
  CaptureHandlers,
  RulesHandlers,
  RequestHandlers,
  JobHandlers,
  SourceHandlers,
  PcapHandlers,
  ReportHandlers,
  CampaignHandlers,
  AttachmentHandlers,
  LinkHandlers
)
