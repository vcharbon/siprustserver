/**
 * The overview of a campaign page. The campaign list links each campaign's
 * overview, a campaign with no run yet among them. One campaign states its
 * runs and links the overview of each case a run failed. With a case open it states everything the page shows of it:
 * the case file (intention, input) and its compiled document, the result of
 * every lane on the run on screen — the `delta` lane first — with its checks,
 * expected beside seen, and per cell the ladder as text (the deployment's
 * trace logs over the cell's window below it), the cell's evidence, its
 * attachments and the bundle's recorded traces; then the case's history over
 * the runs read.
 */
import {
  absolute,
  campaignDelta,
  href,
  linksHref,
  LLMS_PATH,
  type Overview,
  type OverviewLink,
  type OverviewSection
} from "@sip/viewer-core/routes"
import { Effect } from "effect"
import { PageMissing } from "./api.ts"
import { AttachmentReads, fileHref } from "./attachments.ts"
import { Campaign } from "./campaign.ts"
import { CampaignCases } from "./campaign-cases.ts"
import { sutOf } from "./lane-roles.ts"
import { cellPart, sized } from "./links-cell.ts"
import { SitePack } from "./site-pack.ts"

type View = Campaign.View
type Run = View["runs"][number]
type Lane = Run["lanes"][number]
type Result = Lane["results"][number]

/** The most failed cases a campaign overview links. */
const MAX_FAILED = 50

const segment = encodeURIComponent

const integer = (text: string | null): number | undefined => {
  const value = Number(text)
  return text === null || text === "" || !Number.isInteger(value) ? undefined : value
}

const plural = (count: number, noun: string): string => `${count} ${noun}${count === 1 ? "" : "s"}`

const day = (instant: string): string => instant.slice(0, 16).replace("T", " ")

const runLabel = (run: Run): string =>
  `#${run.id} ${day(run.startedAt)} · ${run.labels.map((l) => `${l.name} ${l.value}`).join(" · ")}`

const resultWord = (result: Result | undefined): string =>
  result === undefined
    ? "not run"
    : result.status === "pass"
    ? "pass"
    : `${result.status}${result.checks.length === 0 ? "" : `, ${plural(result.checks.length, "failed check")}`}`

/** One verification line in a line: its name and source, its answer and marks, its message. */
const verificationLine = (line: Result["lines"][number]): string =>
  `- ${line.name} [${line.source}]: ${line.answer}${line.waived ? " (waived)" : line.informative ? " (informative)" : ""}${
    line.message === null ? "" : `: ${line.message}`
  }`

const resultOf = (lane: Lane, caseId: string): Result | undefined => lane.results.find((r) => r.caseId === caseId)

/** One failed check in a line: where, what, whose side, expected beside seen, what the decision said. */
const checkLine = (check: Result["checks"][number], saidLabel: string): string =>
  [
    `- ${check.anchor} · ${check.field}${check.side === null ? "" : ` [${check.side}]`}`,
    `expected: ${check.expected ?? "—"}`,
    `seen: ${check.seen ?? "—"}`,
    ...(check.said === null ? [] : [`${saidLabel}: ${check.said}`])
  ].join(" · ")

/** The page overview of the campaign page at `query`. */
export const campaignOverview = Effect.fn("Links.campaignOverview")(function*(
  base: Overview,
  capture: string | undefined,
  query: URLSearchParams,
  origin: string
) {
  const campaign = yield* Campaign.Service
  if (capture === undefined) {
    const names = yield* campaign.list
    return {
      ...base,
      status: [`${names.length} campaigns`],
      sections: [...base.sections, {
        heading: "Campaigns",
        lines: [],
        links: names.map((name) => ({
          label: `campaign ${name}`,
          href: linksHref(origin, absolute(origin, href.campaign(name)))
        }))
      }]
    } satisfies Overview
  }

  const names = yield* campaign.list
  if (!names.includes(capture)) {
    return yield* new PageMissing({
      reason: `no campaign ${capture}; the campaigns: ${names.length === 0 ? "none" : names.join(", ")}`,
      llms: absolute(origin, LLMS_PATH)
    })
  }
  const delta = campaignDelta(query)
  const caseId = query.get("case") ?? undefined
  const view = yield* campaign.read(capture, {
    env: query.get("env") ?? undefined,
    run: delta?.run ?? integer(query.get("run"))
  })
  const head = view.runs.length === 0 && view.envs.length === 0
    ? [`campaign ${capture} · no run yet`]
    : [
      `campaign ${capture} · env ${view.env ?? "—"} · ${plural(view.runs.length, "run")} read` +
      ` · lanes ${view.lanes.join(", ")}`
    ]
  if (caseId === undefined) {
    return {
      ...base,
      title: `Campaign ${capture}`,
      status: head,
      sections: [...summarySections(view, capture, query, origin), pageData(view, capture, origin)]
    } satisfies Overview
  }

  const run = view.runs.find((r) => r.id === delta?.run) ??
    view.runs.find((r) => r.id === integer(query.get("run"))) ??
    view.runs.find((r) => r.lanes.some((lane) => resultOf(lane, caseId) !== undefined))
  const lanes = run === undefined
    ? []
    : [...run.lanes].sort((a, b) => Number(b.lane === delta?.lane) - Number(a.lane === delta?.lane))
  const laneSections = yield* Effect.forEach(lanes, (lane) =>
    laneSection(view, capture, run!, lane, caseId, lane.lane === delta?.lane, origin))
  const known = view.cases.find((c) => c.caseId === caseId)
  return {
    ...base,
    title: `Campaign case ${caseId}`,
    status: [
      ...head,
      run === undefined
        ? `no run read holds this case`
        : `run #${run.id}: ${run.lanes.map((lane) => `${lane.lane} ${resultWord(resultOf(lane, caseId))}`).join(" · ")}`,
      ...(delta === undefined ? [] : [`on screen: run #${delta.run}, lane ${delta.lane}`])
    ],
    sections: [
      yield* caseSection(caseId, known, origin),
      ...laneSections,
      historySection(view, caseId),
      pageData(view, capture, origin)
    ]
  } satisfies Overview
})

/** The case file and its compiled document; a case no file states says so. */
const caseSection = Effect.fn("Links.campaignCase")(function*(
  caseId: string,
  known: View["cases"][number] | undefined,
  origin: string
) {
  const cases = yield* CampaignCases.Service
  const read = yield* cases.read(caseId).pipe(Effect.option)
  const asked = `?case=${segment(caseId)}`
  return {
    heading: "Case",
    lines: [
      ...(known === undefined
        ? []
        : [`ref: ${known.ref}`, `sub-case: ${known.subcase ?? "—"}`, `family: ${known.family}`]),
      ...(read._tag === "None"
        ? ["case file: none in this checkout states this case"]
        : [
          `case file: ${read.value.file}`,
          ...(read.value.intention.length === 0 ? [] : ["intention:", ...read.value.intention.map((l) => `> ${l}`)])
        ])
    ],
    links: read._tag === "None" ? [] : [
      {
        label: "case source (TypeScript: intention comment, then the input)",
        href: absolute(origin, `/api/campaign-cases/source${asked}`)
      },
      { label: "compiled pivot (JSON, as this checkout compiles it)", href: absolute(origin, `/api/campaign-cases/pivot${asked}`) }
    ]
  } satisfies OverviewSection
})

/**
 * The label of a cell's attachment: its title and kind; the one a check's
 * `said` is read from (`saidLog`), where it holds `records`, as the routing
 * service's records of the cell with their count.
 */
const attachmentLabel = (one: AttachmentReads.Listed, saidLog: string, records: number | undefined): string =>
  one.name === saidLog && records !== undefined
    ? `${one.title}, the routing service's records of this cell (${records} records, JSON lines)`
    : `${one.title} (${one.kind})`

/** One lane's result of the case on `run`: the outcome and checks, and the cell's evidence. */
const laneSection = Effect.fn("Links.campaignLane")(function*(
  view: View,
  capture: string,
  run: Run,
  lane: Lane,
  caseId: string,
  onScreen: boolean,
  origin: string
) {
  const result = resultOf(lane, caseId)
  const heading = `Run #${run.id} · ${lane.lane}: ${resultWord(result)}${onScreen ? " · on screen" : ""}`
  if (result === undefined) {
    return {
      heading,
      lines: [
        `SUT on this lane: ${sutOf(yield* SitePack.Service, lane.lane)}`,
        `lane run ${lane.runId} (${lane.status}) did not run this case`
      ],
      links: []
    }
  }
  const lines = [
    `lane run ${lane.runId} · ${lane.status}`,
    ...(result.detail === null ? [] : [`detail: ${result.detail}`]),
    ...(result.lines.length === 0 ? [] : ["lines:", ...result.lines.map(verificationLine)]),
    ...(result.checks.length === 0 ? [] : ["failed checks:", ...result.checks.map((c) => checkLine(c, view.saidLabel))])
  ]
  if (result.cell === null) return { heading, lines: [...lines, "no cell bundle written"], links: [] }
  const part = yield* cellPart(capture, lane.session, result.cell, lane.lane, { cell: result.cell }, origin, false)
  const reads = yield* AttachmentReads.Service
  const held = yield* reads.list({ scope: "cell", runId: lane.runId, caseId })
  const said = result.log === null
    ? []
    : result.log.missing !== undefined
    ? [`${view.saidLog}: missing (${result.log.missing})`]
    : [`${view.saidLog}: ${result.log.records ?? 0} records`]
  const records = result.log !== null && result.log.missing === undefined ? result.log.records ?? 0 : undefined
  const attached: ReadonlyArray<OverviewLink> = yield* Effect.forEach(held, (one) =>
    Effect.map(
      one.kind === "link" ? Effect.succeed(0) : Effect.orElseSucceed(Effect.map(reads.stored(one.id), (file) => file.bytes), () => 0),
      (bytes) => ({
        label: sized(attachmentLabel(one, view.saidLog, records), bytes),
        href: absolute(origin, one.kind === "link" ? `/api/attachments/${one.id}` : fileHref(one.id))
      })
    ))
  return { heading, lines: [...lines, ...said, ...part.lines], links: [...part.links, ...attached] } satisfies OverviewSection
})

/** The case on every run read, newest first, a line per run. */
const historySection = (view: View, caseId: string): OverviewSection => ({
  heading: `Case history · env ${view.env ?? "—"}`,
  lines: view.runs.flatMap((run) => {
    const seen = run.lanes.filter((lane) => resultOf(lane, caseId) !== undefined)
    return seen.length === 0
      ? []
      : [`- #${run.id} ${day(run.startedAt)}: ${seen.map((l) => `${l.lane} ${resultWord(resultOf(l, caseId))}`).join(" · ")}`]
  }),
  links: []
})

/** The page's own JSON, sized: the campaign's runs in the environment read. */
const pageData = (view: View, capture: string, origin: string): OverviewSection => ({
  heading: "Page data (JSON)",
  lines: [],
  links: [{
    label: sized("campaign runs: every lane's results and failed checks", JSON.stringify(view).length),
    href: absolute(origin, `/api/campaign/${segment(capture)}${view.env === null ? "" : `?env=${segment(view.env)}`}`)
  }]
})

/** A campaign with no case open: its runs, and the overview of each case the newest run failed. */
const summarySections = (
  view: View,
  capture: string,
  query: URLSearchParams,
  origin: string
): ReadonlyArray<OverviewSection> => {
  const runs: OverviewSection = {
    heading: "Runs",
    lines: view.runs.map((run) =>
      `- ${runLabel(run)}: ${
        run.lanes.map((lane) =>
          `${lane.lane} ${lane.results.filter((r) => r.outcome === "pass").length}/${lane.results.length} pass`
        ).join(" · ")
      }`
    ),
    links: []
  }
  const read = view.runs.find((r) => r.id === integer(query.get("run"))) ?? view.runs[0]
  if (read === undefined) return [runs]
  const failed = view.cases.flatMap((c) => {
    const lane = read.lanes.find((l) => {
      const result = resultOf(l, c.caseId)
      return result !== undefined && result.outcome !== "pass"
    })
    return lane === undefined ? [] : [{ caseId: c.caseId, lane: lane.lane }]
  })
  return [runs, {
    heading: `Failed cases of run #${read.id} (${failed.length})`,
    lines: failed.length > MAX_FAILED ? [`the first ${MAX_FAILED} linked`] : [],
    links: failed.slice(0, MAX_FAILED).map((one) => ({
      label: `${one.caseId} (${one.lane})`,
      href: linksHref(
        origin,
        absolute(origin, href.campaign(capture, {
          env: view.env ?? undefined,
          case: one.caseId,
          delta: `${read.id}:${one.lane}`
        }))
      )
    }))
  }]
}
