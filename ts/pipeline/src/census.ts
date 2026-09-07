/**
 * The MERGE that turns a census sweep's source-side hits into allowed-errors
 * entries.
 *
 * `sipflow --rfc-census` decides a rule off the wire and charges an endpoint;
 * `allowed-errors.json` is the committed memory of what a human accepted about
 * a source capture. This module is the one seam between them, and the whole of
 * its policy is one boundary:
 *
 * - a hit whose emitter is on the SOURCE side is mechanical — the registry
 *   gains it without anyone being asked;
 * - a hit charging the PLATFORM, or one no deployment could place, is NEVER
 *   auto-accepted. It is listed for a human ruling, loudly.
 *
 * The side is the CUT's, off its SUT address set: the deployment statement is
 * the one authority on which socket is the platform, and the census's own
 * `emitter_role` — read off group topology, which endpoint several legs cross —
 * is carried in the report as evidence and decides nothing here.
 *
 * Before the side is judged, the CUT gets to speak: a hit no case will ever be
 * asked about is not a question for anyone. Every such reading comes from the
 * caller's {@link CutOracle}, because taking a cut needs the flows document the
 * report only names.
 *
 * Merging never rewrites. An existing entry is carried through verbatim,
 * whoever wrote it, and a hit an entry already covers is a no-op — so the tool
 * is idempotent and a hand-written note is never machine-edited.
 */
import type { AllowedErrors, Census } from "@sip/contracts"
import { chargedSide, type ChargedSide, type SutSet } from "./sut.js"

/** One hit's fate under the merge policy. */
export interface Decision {
  readonly hit: Census.CensusHit
  readonly outcome:
    | "added"
    | "duplicate"
    | "needs-ruling"
    | "outside-cut"
    | "excluded-sut-invalid"
    | "declared-negative"
    | "case-refused"
  /** Which side the cut's SUT set put the charged emitter on. */
  readonly side: ChargedSide
  /** Why, in the words a human reads on stderr. */
  readonly reason: string
}

export interface Merged {
  readonly registry: AllowedErrors.AllowedErrors
  readonly decisions: ReadonlyArray<Decision>
  readonly added: ReadonlyArray<Decision>
  readonly duplicates: ReadonlyArray<Decision>
  readonly needsRuling: ReadonlyArray<Decision>
  readonly outsideCut: ReadonlyArray<Decision>
  readonly sutInvalid: ReadonlyArray<Decision>
  readonly declaredNegative: ReadonlyArray<Decision>
  /**
   * Hits on a call the cut cases but the generator REFUSES: no case will ever
   * carry them, so no entry can anchor. Settled, not asked — the next census
   * re-takes the cut, so a case that later materializes re-surfaces its hits.
   */
  readonly caseRefused: ReadonlyArray<Decision>
}

/** What the cut says about the call a hit sits on, where that settles the hit. */
export interface CutVerdict {
  readonly outcome: "outside-cut" | "excluded-sut-invalid" | "declared-negative" | "case-refused"
  readonly reason: string
}

/**
 * What the CUT reads off one hit: the deployment its capture was cut with, and
 * the verdict where the cut settles the hit outright.
 *
 * `sut` absent means no cut could be taken at all — an unreadable or uncuttable
 * document — so the hit's emitter is placed on NEITHER side. A missing `verdict`
 * means the cut makes a case of the call and the merge policy decides the hit on
 * its own.
 */
export interface CutReading {
  readonly sut?: SutSet
  readonly verdict?: CutVerdict
}

/**
 * The cut, per hit. Supplied by the caller because taking it needs the flows
 * document the report only names.
 */
export type CutOracle = (hit: Census.CensusHit) => CutReading

/**
 * The census report merged into `registry`. Pure: the caller decides whether to
 * write the result, so a dry run and a real one take exactly the same path.
 *
 * One entry per (capture, rule, originator, call-id). Widening an existing
 * entry's `call_ids` would be a rewrite of something a human may have written,
 * so a new call-id gets its own entry instead.
 */
export const mergeCensus = (
  registry: AllowedErrors.AllowedErrors,
  report: Census.CensusReport,
  cut: CutOracle
): Merged => {
  const captures: Record<string, Array<AllowedErrors.AllowedViolation>> = {}
  for (const [capture, entries] of Object.entries(registry.captures)) {
    captures[capture] = entries.map((e) => ({ ...e, call_ids: [...e.call_ids] as [string, ...Array<string>] }))
  }

  const decisions: Array<Decision> = []
  for (const hit of report.hits) {
    // A hit the registry can never be asked about is settled by the cut, not by
    // this policy: the registry only ever stamps a case, so listing it would be
    // a decision with nothing to decide. Each such reading is reported in its
    // own block rather than dropped, and each is tested BEFORE the duplicate
    // check, so an entry a human already wrote that has stopped being anchorable
    // says so rather than reading as settled.
    const reading = cut(hit)
    const side = chargedSide(reading.sut, hit.emitter)
    if (reading.verdict !== undefined) {
      decisions.push({ hit, outcome: reading.verdict.outcome, side, reason: reading.verdict.reason })
      continue
    }
    // Read without creating: a capture whose every hit is refused must not gain
    // an empty entry list and read as a capture someone cleared.
    const entries = captures[hit.capture] ?? []
    // The duplicate test comes FIRST: a hit the registry already carries has
    // been ruled on, whichever side its emitter sits on, and re-asking about it
    // would bury the hits nobody has looked at yet.
    const covered = entries.some(
      (e) => e.rule === hit.rule && e.originator === hit.emitter && e.call_ids.includes(hit.call_id)
    )
    if (covered) {
      decisions.push({
        hit,
        outcome: "duplicate",
        side,
        reason: "the registry already lists this rule, originator and call-id"
      })
      continue
    }
    const guard = refusal(hit, side, reading.sut)
    if (guard !== undefined) {
      decisions.push({ hit, outcome: "needs-ruling", side, reason: guard })
      continue
    }
    captures[hit.capture] = [
      ...entries,
      {
        rule: hit.rule,
        originator: hit.emitter,
        call_ids: [hit.call_id],
        note: noteFor(hit, reading.sut)
      }
    ]
    decisions.push({
      hit,
      outcome: "added",
      side,
      reason: `source-side emitter — the cut's SUT set (${addresses(reading.sut)}) does not hold it`
    })
  }

  const of = (outcome: Decision["outcome"]) => decisions.filter((d) => d.outcome === outcome)
  return {
    registry: { ...registry, captures },
    decisions,
    added: of("added"),
    duplicates: of("duplicate"),
    needsRuling: of("needs-ruling"),
    outsideCut: of("outside-cut"),
    sutInvalid: of("excluded-sut-invalid"),
    declaredNegative: of("declared-negative"),
    caseRefused: of("case-refused")
  }
}

/**
 * `undefined` when the hit may be accepted mechanically, and the refusal's
 * reason otherwise. Only a SOURCE-side emitter flows in: the capture's own SUT
 * is never auto-accepted, and an endpoint no deployment places is not a side.
 */
const refusal = (
  hit: Census.CensusHit,
  side: ChargedSide,
  sut: SutSet | undefined
): string | undefined => {
  if (side === "platform") {
    return (
      `emitter ${hit.emitter} is one of this capture's SUT addresses (${addresses(sut)}) — ` +
      `a SUT-emitted violation is never auto-accepted`
    )
  }
  if (side === "unattributed") {
    return (
      `no SUT set was decided for ${hit.capture}, so ${hit.emitter} sits on neither side of ` +
      `the deployment`
    )
  }
  return undefined
}

/** The SUT set in one clause, for a human reading a refusal or a note. */
const addresses = (sut: SutSet | undefined): string =>
  sut === undefined || sut.size === 0 ? "none" : sut.addresses.join(", ")

/**
 * The entry's note: what the census saw, in the report's own numbers, which
 * deployment made the addition mechanical, and that a tool rather than a human
 * wrote it.
 */
const noteFor = (hit: Census.CensusHit, sut: SutSet | undefined): string =>
  `${hit.emitter} ${describeHit(hit)}. Auto-added by a census merge from a ` +
  `\`sipflow --rfc-census\` sweep (leg ${hit.leg} of the capture's flows document); the cut's ` +
  `SUT set for this capture is ${addresses(sut)}, which does not hold this socket, so the ` +
  `emitter is on the source side and this violation is listed and never gates.`

/**
 * What the census saw, in one clause and in the report's own numbers. Shared,
 * because a registry note and a generation refusal describe the same fact.
 */
export const describeHit = (hit: Census.CensusHit): string => {
  switch (hit.rule) {
    case "no-200-after-cancel":
      return (
        `answered ${hit.status} to the INVITE on CSeq ${hit.cseq} ` +
        `${ms(hit.gap_us)} ms after taking the CANCEL from ${hit.taker}`
      )
    case "unacked-reliable-provisional":
      return (
        `took a reliable ${hit.status} (RSeq ${hit.rseq}) on CSeq ${hit.cseq} from ${hit.taker} ` +
        `and never PRACKed it; the dialog stayed alive ${ms(hit.window_us)} ms longer`
      )
    case "no-ack-to-dialog-creating-2xx":
      return (
        `took the ${hit.status} confirming dialog tag '${hit.to_tag}' on CSeq ${hit.cseq} from ` +
        `${hit.taker} and never ACKed it; the capture ran ${ms(hit.window_us)} ms longer` +
        (hit.retransmits > 0 ? `, through ${hit.retransmits} retransmission(s)` : "") +
        (hit.bye_after_us !== undefined
          ? `, and the dialog was torn down un-confirmed by ${hit.bye_by}` +
            ` ${ms(hit.bye_after_us)} ms later`
          : "")
      )
    case "no-cancel-after-final":
      return (
        `CANCELled the INVITE on CSeq ${hit.cseq} toward ${hit.taker} ` +
        `${ms(hit.since_final_us)} ms after taking its ${hit.final_status} final — and after ` +
        `ACKing that final, so the transaction was already completed`
      )
    case "second-answer-repeats-the-first":
      return (
        `answered the offer on CSeq ${hit.cseq} toward ${hit.taker} a second time on the same ` +
        `dialog with another transport plan — first [${hit.first_plan.join(", ")}], then ` +
        `[${hit.second_plan.join(", ")}], which the peer takes as no answer at all`
      )
  }
}

/** Microseconds as milliseconds, one decimal — the report's own resolution. */
const ms = (us: number): number => Math.round(us / 100) / 10

/** The registry as it goes back to disk: canonical, and stable across runs. */
export const renderRegistry = (registry: AllowedErrors.AllowedErrors): string => {
  const captures = Object.fromEntries(
    Object.keys(registry.captures)
      .sort()
      .map((k) => [k, registry.captures[k]])
  )
  return `${JSON.stringify({ ...registry, captures }, null, 2)}\n`
}

/** A dated census run, as the sweep's own README names them: `2026-08-23b.census.json`. */
const DATED_RUN = /^\d{4}-\d{2}-\d{2}[a-z]?\.census\.json$/

/**
 * The census run a consumer reads when none is named: the LATEST dated report
 * among `files`. `undefined` where none is dated — census reports are working
 * data, not committed, and a checkout without them merges nothing rather than
 * failing.
 *
 * Takes names, never a directory: which tree holds the runs is a deployment's.
 */
export const latestCensusReport = (files: ReadonlyArray<string>): string | undefined =>
  files.filter((f) => DATED_RUN.test(f)).sort().reverse()[0]
