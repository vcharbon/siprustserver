/**
 * The leg joins the `header-pair` correlation rules (`@sip/contracts`
 * `Rules.HeaderPairRule`) state over a capture, for the cut's correlation
 * (`./cut.ts`).
 *
 * A leg's anchor is its initial INVITE: the first dialog-creating INVITE it
 * carries, by capture order. A rule's `Call-ID` side is the leg's own Call-ID;
 * any other header is read off the extractor's projection (`msgs[].headers`),
 * never off the bytes, so a rule naming a header the document does not
 * project either refuses the document or is skipped, as its {@link LegRules}
 * say, and is never read as joining nothing.
 *
 * Linear in the legs: each rule indexes every leg's `left` keys once, then
 * looks each leg's `right` keys up in that index; under a window, each key's
 * left legs are sorted by time once and the nearest is found by bisection.
 */
import { Flows, Rules } from "@sip/contracts"
import { isDialogCreatingInvite } from "./sut.js"

/**
 * The `header-pair` rules a cut joins a capture's legs by, and what a flows
 * document that does not project a header one of them names gets.
 */
export interface LegRules {
  readonly rules: ReadonlyArray<Rules.HeaderPairRule>
  /**
   * `require`: such a document is refused, {@link UnprojectedHeaders} thrown.
   * `skip`: the rule joins nothing in it, and the joins name it
   * ({@link HeaderPairJoins.skipped}).
   */
  readonly unprojected: "require" | "skip"
}

/** No leg rule: the cut joins legs by the Call-ID derivation alone. */
export const NO_LEG_RULES: LegRules = { rules: [], unprojected: "require" }

/** A leg rule left out of a document's joins, and the headers it reads that the document does not project. */
export interface SkippedRule {
  readonly rule: string
  readonly missing: ReadonlyArray<string>
}

/** A flows document does not project `missing`, headers the required leg rule `rule` reads. */
export class UnprojectedHeaders extends Error {
  readonly rule: string
  readonly missing: ReadonlyArray<string>

  constructor(rule: string, missing: ReadonlyArray<string>, projected: ReadonlyArray<string>) {
    super(`the flows document projects [${projected.join(", ")}]; the leg rule "${rule}" reads ${missing.join(", ")}`)
    this.rule = rule
    this.missing = missing
  }
}

/** One join a rule states: a leg was set up off the leg `base`, by the rule named `rule`. */
export interface RuleJoin {
  readonly base: number
  readonly rule: string
}

/** What a capture's leg rules state over it. */
export interface HeaderPairJoins {
  /** Per leg, every join making it a rule's right side; one per base and rule. */
  readonly joins: ReadonlyArray<ReadonlyArray<RuleJoin>>
  /** The rules a `skip` set left out, the document projecting not their headers. */
  readonly skipped: ReadonlyArray<SkippedRule>
}

/** A leg's initial INVITE time, under one key. */
interface Anchor {
  readonly leg: number
  readonly ts_us: number
}

/** Every key `re` yields over `values`, in their order. */
const keysOf = (re: RegExp, values: ReadonlyArray<string>): ReadonlyArray<string> =>
  values.flatMap((value) => {
    const key = re.exec(value)?.groups?.key
    return key === undefined ? [] : [key]
  })

/**
 * The anchor of `anchors` (ascending by time) other than leg `self` whose time
 * is nearest `ts_us`, within `window_us` of it; the earlier on a tie.
 */
const nearest = (anchors: ReadonlyArray<Anchor>, ts_us: number, self: number, window_us: number): Anchor | undefined => {
  let lo = 0
  let hi = anchors.length
  while (lo < hi) {
    const mid = (lo + hi) >>> 1
    if (anchors[mid]!.ts_us <= ts_us) lo = mid + 1
    else hi = mid
  }
  let down = lo - 1
  while (down >= 0 && anchors[down]!.leg === self) down--
  let up = lo
  while (up < anchors.length && anchors[up]!.leg === self) up++
  const before = anchors[down]
  const after = anchors[up]
  const best = before === undefined || (after !== undefined && after.ts_us - ts_us < ts_us - before.ts_us) ? after : before
  return best !== undefined && Math.abs(best.ts_us - ts_us) <= window_us ? best : undefined
}

/** The headers of `rule` `flows` does not project, `Call-ID` being the leg's own. */
const missingOf = (flows: Flows.FlowsDoc, rule: Rules.HeaderPairRule): ReadonlyArray<string> => {
  const have = new Set(flows.emit_headers.map((h) => h.toLowerCase()))
  return Rules.headersNamedBy([rule]).filter((h) => !have.has(h.toLowerCase()))
}

/**
 * The joins `legRules` state over `flows`: a leg whose `right` header yields a
 * key joins, as the right side, the other legs whose `left` header yields it:
 * every one of them without a window, the one whose initial INVITE is nearest
 * in time within `window_ms` with it. Throws {@link UnprojectedHeaders} where
 * `flows` does not project a header a rule of a `require` set reads.
 */
export const headerPairJoins = (flows: Flows.FlowsDoc, legRules: LegRules): HeaderPairJoins => {
  const skipped: Array<SkippedRule> = []
  const applied = legRules.rules.filter((rule) => {
    const missing = missingOf(flows, rule)
    if (missing.length === 0) return true
    if (legRules.unprojected === "require") throw new UnprojectedHeaders(rule.name, missing, flows.emit_headers)
    skipped.push({ rule: rule.name, missing })
    return false
  })
  const joins: Array<Array<RuleJoin>> = flows.legs.map(() => [])
  if (applied.length === 0) return { joins, skipped }
  const invites = flows.legs.map((leg) => leg.msgs.find(isDialogCreatingInvite))
  const sideKeys = (i: number, side: Rules.HeaderSide, re: RegExp): ReadonlyArray<string> => {
    const invite = invites[i]
    return invite === undefined ? [] : keysOf(re, Flows.ruleHeaderValues(flows.legs[i]!.call_id, invite, side.header))
  }
  for (const rule of applied) {
    const left = new RegExp(rule.left.pattern)
    const right = new RegExp(rule.right.pattern)
    const byKey = new Map<string, Array<Anchor>>()
    invites.forEach((invite, i) => {
      for (const key of sideKeys(i, rule.left, left)) {
        const anchor = { leg: i, ts_us: invite!.ts_us }
        const anchors = byKey.get(key)
        if (anchors === undefined) byKey.set(key, [anchor])
        else anchors.push(anchor)
      }
    })
    const window_us = rule.window_ms === undefined ? undefined : rule.window_ms * 1_000
    if (window_us !== undefined) for (const anchors of byKey.values()) anchors.sort((a, b) => a.ts_us - b.ts_us)
    invites.forEach((invite, i) => {
      const bases = new Set<number>()
      for (const key of sideKeys(i, rule.right, right)) {
        const anchors = byKey.get(key) ?? []
        if (window_us === undefined) {
          for (const anchor of anchors) if (anchor.leg !== i) bases.add(anchor.leg)
        } else {
          const near = nearest(anchors, invite!.ts_us, i, window_us)
          if (near !== undefined) bases.add(near.leg)
        }
      }
      for (const base of bases) joins[i]!.push({ base, rule: rule.name })
    })
  }
  return { joins, skipped }
}
