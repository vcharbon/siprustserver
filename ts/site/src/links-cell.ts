/**
 * One cell as a page overview states it, whatever page it is on: the lane's
 * SUT, the gate that decided the cell, its failing findings each on the
 * ladder message it points at, its SUT records; then its digest, its ladder
 * text and the text with every payload unfolded, its whole evidence and its
 * recorded traces, each link over a few kilobytes labelled with its size.
 */
import type { OverviewLink } from "@sip/viewer-core/routes"
import { Effect } from "effect"
import { digestOf, sizeOf } from "./cell-digest.ts"
import { sutOf } from "./lane-roles.ts"
import { RunText } from "./run-text.ts"
import { RunTrace } from "./run-trace.ts"
import { SitePack } from "./site-pack.ts"
import { TriageCatalog } from "./triage.ts"

/** Which run text the cell is read in: narrowed to its call (`case`) or to itself (`cell`). */
export type Narrowing = { readonly case: string } | { readonly cell: string }

const segment = encodeURIComponent

/** A label with the size of what it links, where that is more than a few kilobytes. */
export const sized = (label: string, bytes: number): string => (bytes < 4096 ? label : `${label}, ${sizeOf(bytes)}`)

/** The lines and links of the cell `run`/`cell` of `capture` on `lane`. */
export const cellPart = Effect.fn("Links.cell")(function*(
  capture: string,
  run: string,
  cell: string,
  lane: string,
  narrowing: Narrowing,
  origin: string,
  /** Whether the verdict's failures are stated: a page that states them its own way says no. */
  failures = true,
  /** The flow step the page is on: its messages are linked. */
  step?: string
) {
  const triage = yield* TriageCatalog.Service
  const traces = yield* RunTrace.Service
  const runText = yield* RunText.Service
  const pack = yield* SitePack.Service
  const sut = sutOf(pack, lane)
  const cellPath =`/api/triage/${segment(capture)}/runs/${segment(run)}/cells/${segment(cell)}`
  const textQuery = "case" in narrowing ? `case=${segment(narrowing.case)}` : `cell=${segment(narrowing.cell)}`
  const textPath = `/api/triage/${segment(capture)}/runs/${segment(run)}/text?${textQuery}&lane=${segment(lane)}`
  const textSize = (full: boolean) =>
    runText.text(capture, run, lane, full, "case" in narrowing ? narrowing.case : undefined, "cell" in narrowing ? narrowing.cell : undefined)
      .pipe(Effect.map((body) => Buffer.byteLength(body)), Effect.exit)
  const [brief, whole] = yield* Effect.all([textSize(false), textSize(true)], { concurrency: 2 })
  const textLinks: ReadonlyArray<OverviewLink> = [
    {
      label: sized("ladder as text (verdict, ladders, the SUT's log lines of this call)", brief._tag === "Success" ? brief.value : 0),
      href: `${origin}${textPath}`
    },
    {
      label: sized("ladder as text with every SIP body and HTTP exchange, logs whole", whole._tag === "Success" ? whole.value : 0),
      href: `${origin}${textPath}&full=1`
    }
  ]
  const detail = yield* triage.cell(capture, run, cell).pipe(Effect.exit)
  if (detail._tag === "Failure") {
    return { lines: [`SUT on this lane: ${sut}`, "no cell evidence on disk"], links: textLinks }
  }
  // A cell whose evidence the ladder cannot be drawn from still gets its links.
  const read = yield* Effect.sync(() =>
    digestOf({ capture, run, lane, detail: detail.value, logs: [], pack }, origin)
  ).pipe(Effect.exit)
  if (read._tag === "Failure") {
    return { lines: [`SUT on this lane: ${sut}`, "the cell's evidence does not read"], links: textLinks }
  }
  const digest = read.value
  const failing = digest.findings.filter((f) => f.standing === "gating")
  const recorded = yield* traces.list(capture, run, cell).pipe(
    Effect.orElseSucceed(() => [] as ReadonlyArray<RunTrace.Trace>)
  )
  return {
    lines: [
      `SUT on this lane: ${sut}`,
      `outcome: ${digest.outcome}`,
      ...(!failures || digest.failures.length === 0 ? [] : ["verdict failures:", ...digest.failures.map((f) => `- ${f}`)]),
      ...(failing.length === 0 ? [] : [
        "gating RFC findings:",
        ...failing.map((f) =>
          `- ${f.rule} · charged ${f.charged}${f.at === undefined ? "" : ` · at ${f.at.line}`}` +
          `${f.note === undefined ? "" : ` · ${f.note}`} · ${f.detail}`
        )
      ]),
      ...(digest.records === undefined ? [] : [`SUT records: ${digest.records}`])
    ],
    links: [
      {
        label: "cell digest: gates, every RFC finding on its message, lane facts, routing exchanges, SUT log lines of this call",
        href: `${origin}${cellPath}/digest?lane=${segment(lane)}`
      },
      ...(step === undefined ? [] : [{
        label: `step ${step} on the page: its messages, the case step and the captured message it cites`,
        href: `${origin}${cellPath}/message?step=${segment(step)}`
      }]),
      ...failing.flatMap((f) =>
        f.at === undefined
          ? []
          : [{
            label: `message m${f.at.m}${f.at.step === undefined ? "" : ` (step ${f.at.step})`} of ${f.rule}, headers and body`,
            href: f.at.href
          }]
      ),
      ...textLinks,
      {
        label: sized("cell evidence, whole (JSON: verdict, RFC audit, pivot, recording, HTTP exchanges, SUT records)", JSON.stringify(detail.value).length),
        href: `${origin}/api/triage/${segment(capture)}/cells/${segment(run)}/${segment(cell)}`
      },
      ...recorded.map((trace) => ({
        label: sized(`${trace.file} (${trace.kind})`, trace.bytes),
        href: `${origin}${cellPath}/trace?file=${segment(trace.file)}`
      }))
    ] satisfies ReadonlyArray<OverviewLink>
  }
})
