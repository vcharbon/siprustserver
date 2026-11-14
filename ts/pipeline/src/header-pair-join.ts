/**
 * The leg joins the `header-pair` correlation rules (`@sip/contracts`
 * `Rules.HeaderPairRule`) state over a capture, for the cut's correlation
 * (`./cut.ts`).
 *
 * A leg's anchor is its initial INVITE: the first dialog-creating INVITE it
 * carries, by capture order. A rule's headers are read off the extractor's
 * projection (`msgs[].headers`), never off the bytes, so a document that does
 * not project a header a rule names is refused rather than read as joining
 * nothing.
 *
 * Linear in the legs: each rule indexes every leg's `left` keys once, then
 * looks each leg's `right` keys up in that index.
 */
import { Flows, Rules } from "@sip/contracts"
import { isDialogCreatingInvite } from "./sut.js"

/** Every key `re` yields over `values`, in their order. */
const keysOf = (re: RegExp, values: ReadonlyArray<string>): ReadonlyArray<string> =>
  values.flatMap((value) => {
    const key = re.exec(value)?.groups?.key
    return key === undefined ? [] : [key]
  })

/** The keys `side` reads off `leg`'s initial INVITE; none where the leg carries no such INVITE. */
const sideKeys = (leg: Flows.Leg, side: Rules.HeaderSide, re: RegExp): ReadonlyArray<string> => {
  const invite = leg.msgs.find(isDialogCreatingInvite)
  return invite === undefined ? [] : keysOf(re, Flows.headerValues(invite, side.header))
}

/**
 * For each leg of `flows`, the other legs `rules` join it to as the RIGHT
 * side: the legs whose `left` header yields a key its `right` header yields.
 * A key several legs yield joins every one of them. Throws where `flows` does
 * not project a header a rule names.
 */
export const headerPairBases = (
  flows: Flows.FlowsDoc,
  rules: ReadonlyArray<Rules.HeaderPairRule>
): ReadonlyArray<ReadonlyArray<number>> => {
  Flows.requireHeaders(flows, Rules.headersNamedBy(rules))
  const bases: Array<Array<number>> = flows.legs.map(() => [])
  for (const rule of rules) {
    const left = new RegExp(rule.left.pattern)
    const right = new RegExp(rule.right.pattern)
    const byKey = new Map<string, Array<number>>()
    flows.legs.forEach((leg, i) => {
      for (const key of sideKeys(leg, rule.left, left)) {
        const legs = byKey.get(key)
        if (legs === undefined) byKey.set(key, [i])
        else if (legs[legs.length - 1] !== i) legs.push(i)
      }
    })
    flows.legs.forEach((leg, i) => {
      for (const key of sideKeys(leg, rule.right, right)) {
        for (const base of byKey.get(key) ?? []) {
          if (base !== i && !bases[i]!.includes(base)) bases[i]!.push(base)
        }
      }
    })
  }
  return bases
}
