/**
 * What the base states to the campaign runner for a pack that ships no run
 * verb of its own (`@sip/campaign-runner` `CampaignPack`, `ReportPack`): the
 * verb `campaign run`, no check it types itself, its typer of a failed cell
 * ({@link TYPER}: one check per verdict failure and per gating RFC finding,
 * typed to no side), a cell's claims read off its document alone, no
 * pre-check asked by name, `sut-log` the collector whose line is a cell's log,
 * no lane timer, no universe (every case a pack's case sources yield is the
 * campaign), and a JUnit report naming each case by its ref.
 */
import type { CampaignPack } from "@sip/campaign-runner/pack"
import type { Cell } from "@sip/campaign-runner/cell"
import { failureLine } from "@sip/campaign-runner/judge"
import type { Typer } from "@sip/campaign-runner/judge"
import type { ReportPack } from "@sip/campaign-runner/report"
import type { StoredRun } from "@sip/store/campaign-runs"
import type { Check } from "@sip/store/campaign-results"
import * as Effect from "effect/Effect"

/** The collector whose last line of a cell is the log the run keeps of its system under test. */
export const LOG_COLLECTOR = "sut-log"

const text = (value: unknown): string | null =>
  value === undefined || value === null ? null : typeof value === "string" ? value : JSON.stringify(value)

/** A verdict failure as a check: at its site (its kind where it names none), its field, expected and observed, typed to no side. */
const failureCheck = (failure: Readonly<Record<string, unknown>>, at: number): Check => ({
  anchor: text(failure["site"]) ?? text(failure["failure"]) ?? "verdict",
  field: text(failure["field"]) ?? "*",
  expected: text(failure["expected"]),
  seen: text(failure["observed"]) ?? (failure["failure"] === "check-failed" ? null : failureLine(failure)),
  said: null,
  side: null,
  failure: at
})

/** The base's typer: one check per verdict failure, then one per gating RFC finding, each typed to no side. */
export const TYPER: Typer = ({ failures, rfc }) =>
  Effect.succeed([
    ...failures.map(failureCheck),
    ...rfc.filter((finding) => finding.gating).map((finding): Check => ({
      anchor: "rfc",
      field: finding.rule,
      expected: "no gating finding",
      seen: finding.detail,
      said: null,
      side: null
    }))
  ])

/** The base's campaign pack. */
export const CAMPAIGN: CampaignPack<Cell> = {
  verb: "campaign run",
  own: [],
  typer: TYPER,
  claimsOf: () => ({}),
  preChecks: [],
  lock: "siprig-campaign-run",
  logCollector: LOG_COLLECTOR,
  timerKeys: {},
  quickUnrun: []
}

/** The base's report: no universe, each case's testcase class its ref's first segment, no property beyond the runner's. */
export const REPORT: ReportPack<StoredRun> = {
  failureType: (sides) => (sides.some((side) => side !== null) ? sides.filter((side) => side !== null).join(",") : "failure"),
  classOf: (ref) => `campaign.${ref.split("/")[0] ?? ref}`,
  runClass: "campaign.run",
  said: "the backend",
  log: "system log",
  properties: () => [],
  universe: {
    where: Effect.succeed(""),
    refs: () => Effect.succeed([]),
    absent: () => "the base states no universe",
    looked: "the pack's case sources"
  },
  quickNotes: []
}

export * as BaseCampaignPack from "./base-campaign-pack.ts"
