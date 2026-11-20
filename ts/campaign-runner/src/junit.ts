/**
 * The JUnit report of one stored campaign run (`@sip/store` `CampaignResults.CampaignRun`):
 * one `<testsuite>` per lane, one `<testcase>` per case result (`classname`
 * the pack's class of its ref, `name` the case id: the ref and its sub-case),
 * each as its cell status reads in JUnit (`@sip/contracts` `Verification.junitOf`).
 *
 * - `<failure type>` for a cell its lines fail, typed by the pack's type of
 *   the sides of the lines failing it (`line-text.ts`). Its message names
 *   every failing line with its side; its body gives each one's expected and
 *   seen (and what the backend said, for a failed check), then the bundle and
 *   the delta link.
 * - `<error>` for what verified no call whole: a cell the run ruled an error,
 *   an `unverified` one (naming its lines not checkable), one still verifying
 *   at the report, and a lane whose run did not reach its cells (its run
 *   testcase, of the pack's run class).
 * - `<skipped>` for a cell that did not run, with its reason (`not ported`).
 *
 * `<system-out>` names the bundle, its waived lines, and attaches its verdict
 * and log (`[[ATTACHMENT|…]]`), or says why the log is missing.
 */
import { Verification } from "@sip/contracts"
import * as path from "node:path"
import * as CheckLines from "./check-lines.js"
import { type FailureType, labelOf, sideOf, typeOf, valuesOf } from "./line-text.js"
import * as Results from "@sip/store/campaign-results"

/** What a pack states of its JUnit report of a run `R`. */
export interface JunitPack<R extends Results.CampaignRun = Results.CampaignRun> {
  /** The type of a failure off its failing lines' sides. */
  readonly failureType: FailureType
  /** A case's testcase class, off its ref. */
  readonly classOf: (ref: string) => string
  /** The class of a lane's run testcase, for a lane whose run did not reach its cells. */
  readonly runClass: string
  /** How a failed check's body names what the backend said of its field. */
  readonly said: string
  /** How the report names a cell's log (`<log> missing: …`). */
  readonly log: string
  /** The run's properties the pack adds after its seed, each by name; a null value is left out. */
  readonly properties: (run: R) => ReadonlyArray<readonly [string, string | null]>
}

export interface Options {
  /** The pack's run verb (`CampaignPack.verb`): its words joined by `-` name the suite's run property. */
  readonly verb: string
  /** The triage site the delta links point at. */
  readonly site: string
  /** Each bundle's log file, as its attachments list it; a log no entry names is not attached. */
  readonly logs?: ReadonlyMap<string, string>
}

const escape = (text: string): string =>
  text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, "")

const firstLine = (text: string): string => text.split("\n")[0] ?? ""

/** The summary page's link to one case of one run on one lane. */
export const deltaLink = (site: string, run: Results.CampaignRun, lane: string, caseId: string): string =>
  `${site}/campaign/${encodeURIComponent(run.capture)}?run=${run.id}&lane=${encodeURIComponent(lane)}&case=${
    encodeURIComponent(caseId)
  }`

type Line = Verification.Verification

const side = (line: Line): string => sideOf(line) ?? "untyped"

/** One failing line in a failure's body: a failed check with what the backend said, any other line with what it states. */
const bodyOf = (pack: JunitPack<never>, line: Line): string => {
  const check = CheckLines.checkOf(line)
  if (check !== undefined) {
    return [
      `  [${side(line)}] ${line.name}: ${check.anchor}  ${check.field}`,
      `      expected  ${check.expected ?? "(no value)"}`,
      `      seen      ${check.seen ?? "(absent)"}`,
      `      ${pack.said.padEnd(8)}  ${check.said ?? "—"}`
    ].join("\n")
  }
  const { expected, seen } = valuesOf(line)
  return [
    `  [${side(line)}] ${labelOf(line)}`,
    ...(expected === null ? [] : [`      expected  ${expected}`]),
    ...(seen === null ? [] : [`      seen      ${seen}`])
  ].join("\n")
}

const failureOf = <R extends Results.CampaignRun>(
  pack: JunitPack<R>,
  run: R,
  lane: Results.Lane,
  result: Results.Result,
  bundle: string | undefined,
  options: Options
): string => {
  const lines = Verification.failing(result.verifications)
  const type = typeOf(pack.failureType, result.verifications)
  const message = `[${type}] ${lines.length} line(s) failed: ${lines.map((line) => `[${side(line)}] ${labelOf(line)}`).join(", ")}`
  const body = [
    `${lines.length} line(s) failed:`,
    ...lines.map((line) => bodyOf(pack, line)),
    ...(bundle === undefined ? [] : [`bundle: ${bundle}`]),
    `delta:  ${deltaLink(options.site, run, lane.lane, result.caseId)}`
  ].join("\n")
  return `      <failure type="${type}" message="${escape(message)}">${escape(body)}</failure>`
}

/** Why a cell that ran was verified by nothing whole: its lines not checkable, or still pending at the report. */
const unverifiedOf = (status: Verification.CellStatus, result: Results.Result): string => {
  const deciding = result.verifications.filter(Verification.decides)
  const open = (answer: Verification.Answer) => deciding.filter((line) => line.answer === answer).map(labelOf)
  const detail = status === "ok so far"
    ? `still verifying at the report: ${open("pending").join(", ")}`
    : deciding.length === 0
    ? "unverified: no line decides the cell"
    : `unverified: ${open("not-checkable").join(", ")}`
  return `      <error message="${escape(firstLine(detail))}">${escape(detail)}</error>`
}

const systemOut = (
  pack: JunitPack<never>,
  result: Results.Result,
  bundle: string | undefined,
  options: Options
): string | undefined => {
  const waived = result.verifications.filter((line) => line.waived === true).map((line) => `waived: ${labelOf(line)}`)
  const unchecked = result.skipped !== null && Results.ranUnchecked(result.skipped) ? [result.skipped] : []
  if (bundle === undefined && waived.length === 0 && unchecked.length === 0) return undefined
  const lines = [...(bundle === undefined ? [] : [`bundle: ${bundle}`]), ...unchecked, ...waived]
  if (bundle !== undefined) lines.push(`[[ATTACHMENT|${path.join(bundle, "verdict.json")}]]`)
  const log = result.log
  const logFile = bundle === undefined ? undefined : options.logs?.get(bundle)
  if (logFile !== undefined && log !== null && "records" in log) lines.push(`[[ATTACHMENT|${logFile}]]`)
  if (bundle !== undefined && log !== null && "missing" in log) lines.push(`${pack.log} missing: ${log.missing}`)
  return `      <system-out>${escape(lines.join("\n"))}</system-out>`
}

const testcase = <R extends Results.CampaignRun>(
  pack: JunitPack<R>,
  run: R,
  lane: Results.Lane,
  result: Results.Result,
  options: Options
): string => {
  const { ref } = Results.caseNameOf(result.caseId)
  const bundle = result.cell === null ? undefined : path.join(lane.bundlePath, result.cell)
  const status = Results.statusOf(result)
  const body: Array<string> = []
  switch (Results.outcomeOf(result)) {
    case "failure":
      body.push(failureOf(pack, run, lane, result, bundle, options))
      break
    case "error": {
      if (status !== "error") {
        body.push(unverifiedOf(status, result))
        break
      }
      const detail = result.errorDetail ?? "no call was judged"
      body.push(`      <error message="${escape(firstLine(detail))}">${escape(detail)}</error>`)
      break
    }
    case "skipped":
      body.push(
        `      <skipped message="${escape(result.skipped === Results.NOT_PORTED ? status : result.skipped ?? status)}"/>`
      )
      break
    case "pass":
      break
  }
  const out = systemOut(pack, result, bundle, options)
  if (out !== undefined) body.push(out)
  return [
    `    <testcase classname="${escape(pack.classOf(ref))}" name="${escape(result.caseId)}">`,
    ...body,
    "    </testcase>"
  ].join("\n")
}

const property = (name: string, value: string | null): string =>
  value === null ? "" : `      <property name="${escape(name)}" value="${escape(value)}"/>`

const suite = <R extends Results.CampaignRun>(pack: JunitPack<R>, run: R, lane: Results.Lane, options: Options): string => {
  const cases = lane.results.map((r) => testcase(pack, run, lane, r, options))
  const count = (outcome: Results.Outcome) => lane.results.filter((r) => Results.outcomeOf(r) === outcome).length
  let errors = count("error")
  if (lane.status === "error") {
    const detail = lane.detail ?? "the lane's run did not reach its cells"
    cases.push(
      [
        `    <testcase classname="${escape(pack.runClass)}" name="${escape(`${lane.lane} run`)}">`,
        `      <error message="${escape(firstLine(detail))}">${escape(detail)}</error>`,
        "    </testcase>"
      ].join("\n")
    )
    errors += 1
  }
  const properties = [
    property(options.verb.split(" ").join("-"), String(run.id)),
    property("run", String(lane.runId)),
    property("env", run.env),
    property("seed", run.seed),
    ...pack.properties(run).map(([name, value]) => property(name, value)),
    ...lane.components.map((c) => property(c.name, c.sha === null ? null : `${c.sha}${c.dirty === true ? "+dirty" : ""}`))
  ].filter((line) => line !== "")
  return [
    `  <testsuite name="${escape(lane.lane)}" tests="${cases.length}" failures="${count("failure")}" errors="${errors}" skipped="${
      count("skipped")
    }">`,
    "    <properties>",
    ...properties,
    "    </properties>",
    ...cases,
    "  </testsuite>"
  ].join("\n")
}

/** The JUnit document of `run` under `pack`. */
export const junitXml = <R extends Results.CampaignRun>(pack: JunitPack<R>, run: R, options: Options): string =>
  [
    `<?xml version="1.0" encoding="UTF-8"?>`,
    `<testsuites name="${escape(run.capture)}">`,
    ...run.lanes.map((lane) => suite(pack, run, lane, options)),
    "</testsuites>",
    ""
  ].join("\n")
