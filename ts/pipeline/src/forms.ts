/**
 * The written forms of every number a capture carries — what
 * `topology.*.forms` states for a party — harvested from every message once.
 */
import type { Flows } from "@sip/contracts"
import { classKey, type Plan } from "./plan.js"
import { headText, uriUser } from "./wire.js"

/**
 * Keyed by the number's class key. Built ONCE per capture: every message of the
 * document is harvested to fill it and a case's parties are looked up in it; a
 * per-identity walk over the document is quadratic in calls.
 */
export type FormsTable = ReadonlyMap<string, ReadonlyArray<string>>

export const formsTable = (flows: Flows.FlowsDoc, plan: Plan): FormsTable => {
  const forms = new Map<string, Set<string>>()
  for (const leg of flows.legs) {
    for (const msg of leg.msgs) {
      for (const user of harvestNumbers(msg)) {
        const cls = plan.classify(user)
        if (!cls) continue
        const label = plan.formLabel(user)
        if (!label) continue
        const key = classKey(cls)
        const set = forms.get(key)
        if (set === undefined) forms.set(key, new Set([label]))
        else set.add(label)
      }
    }
  }
  return new Map([...forms].map(([key, set]) => [key, [...set].sort()]))
}

/**
 * Number-bearing user-parts of a datagram. A coarse stand-in for the Rust
 * `identities::harvest` seam: the R-URI plus every sip/sips/tel URI in an
 * identity header. It only feeds `forms`, so over-reach costs a label, never a
 * mapping.
 */
const harvestNumbers = (m: Flows.Msg): Array<string> => {
  const out: Array<string> = []
  if (m.summary.kind === "request" && m.summary.uri) out.push(uriUser(m.summary.uri))
  for (const uri of headText(m).split(/\r?\n/).slice(0, 60).flatMap(identityUris)) {
    out.push(uriUser(uri))
  }
  return out.filter((u) => u.length > 0)
}

const IDENTITY_HEADERS =
  /^(from|f|to|t|contact|m|p-asserted-identity|p-preferred-identity|diversion|remote-party-id|history-info|refer-to|r|referred-by|b)\s*:/i

const identityUris = (line: string): Array<string> => {
  if (!IDENTITY_HEADERS.test(line)) return []
  return [...line.matchAll(/(?:sips?|tel):[^>\s,;]+/gi)].map((m) => m[0]!)
}
