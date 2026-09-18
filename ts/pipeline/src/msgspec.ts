/**
 * What one captured datagram becomes as a step's `msg`: the three-tier model
 * applied to a message at one actor's vantage.
 *
 * Tier 1 is stack-owned and omitted, tier 2 is the dialog-opening INVITE's
 * identities turned into positional refs, and EVERYTHING else is tier 3 and
 * freezes verbatim in wire order. A `send` carries the body it emits; an
 * `expect` carries the shape it checks and the existence checks a session timer
 * needs.
 */
import { Case, Flows, Msg } from "@sip/contracts"
import { carriesBody, decompose, expectBody, type ResourceFile } from "./bodies.js"
import { calleeIdentity, CALLER_IDENTITY } from "./calls.js"
import type { MsgSpecDraft } from "./draft.js"
import type { PartsIndex } from "./parts.js"
import { classKey, type Plan } from "./plan.js"
import { peerSide, type ActorObs, type Layout } from "./topology.js"
import { addrForm, hasHeader, headersInOrder, sameHeader, uriUser } from "./wire.js"

/**
 * Tier-1 headers (regenerated from dialog state) plus the ones represented
 * elsewhere (From/To/R-URI as tier-2 refs, Content-Type/-Length via the body).
 * EVERYTHING else is tier-3 and freezes verbatim.
 */
export const OMIT_HEADERS = [
  "Via",
  "Call-ID",
  "CSeq",
  "From",
  "To",
  "Contact",
  "Max-Forwards",
  "Content-Length",
  "Content-Type",
  "Route",
  "Record-Route"
]

/**
 * Headers whose VALUE the replayed stack mints per leg (RFC 3262 §3: each
 * dialog's reliable-provisional sequence is its UAS's own), so an expect states
 * existence via `headers-present` and the confrontation's `rseq-stack-owned`
 * rule judges the number. A send keeps the frozen captured value — the scripted
 * endpoint must put a concrete RSeq on the wire for RAck translation.
 */
export const STACK_OWNED_EXPECT_HEADERS = ["RSeq"]

/**
 * Headers the replayed stack DERIVES from the message it answers, so no side of
 * the document may state one: `RAck` is the provisional's own `RSeq` plus the
 * CSeq of the request it answers (RFC 3262 §7.2), and both numbers belong to
 * the run, not to the capture. A frozen one WINS over the composed value
 * (`stack.rs::prack`), which puts the captured platform's numbers on the wire.
 *
 * An expect states existence instead: §7.2 makes `RAck` mandatory on a PRACK,
 * so a PRACK without one is malformed however the peer numbers.
 */
export const STACK_DERIVED_HEADERS = ["RAck"]

/**
 * Headers whose value states when THIS message was sent (RFC 3261 §20.17 /
 * §20.38): the stack that mints a message writes its own, and a relay carries no
 * peer's — so the captured value is the origin platform's clock and can hold on
 * no replay, and a stack that mints none at all is equally compliant. Dropped
 * from an expect entirely — neither frozen nor existence-checked. A send keeps
 * the captured value, so the SUT is handed what the capture handed the origin.
 */
export const SENDER_CLOCK_HEADERS = ["Date", "Timestamp"]

/**
 * Headers that describe the message BODY (RFC 3261 §20.11 Content-Disposition,
 * §20.12 Content-Encoding, §20.13 Content-Language, §20.24 MIME-Version): they
 * state properties of octets, so on a message that carries none they describe
 * nothing. An expect on a bodyless message freezes none of them — a relay that
 * drops a body drops what described it, and the origin platform leaving one
 * behind is its own spelling, not a fact about the message. A send keeps the
 * captured bytes.
 */
export const BODY_DESCRIPTOR_HEADERS = [
  "Content-Disposition",
  "Content-Encoding",
  "Content-Language",
  "MIME-Version"
]

/**
 * A TRANSACTION-DERIVED message: 100 Trying, ACK, and PRACK with its 2xx. The
 * stack derives its R-URI, Route set, Via and CSeq from the TRANSACTION that
 * obliged it rather than from dialog state — an ACK to a non-2xx reuses the
 * INVITE's own Via branch and Route set (RFC 3261 §17.1.1.3), an ACK to a 2xx
 * takes the outstanding INVITE's CSeq (§13.2.2.4), a PRACK takes its `RAck`
 * from the provisional received (RFC 3262 §7.2), a 100 echoes the request's.
 *
 * It says nothing about STORAGE: such a step stores what any step stores (§6.3).
 */
export const isAutomatic = (m: Flows.Msg): boolean => {
  if (m.summary.kind === "response") {
    const st = m.summary.status
    return st === 100 || (st >= 200 && st < 300 && m.summary.cseq.method.toUpperCase() === "PRACK")
  }
  const method = m.summary.method.toUpperCase()
  return method === "ACK" || method === "PRACK"
}

/** Cross-leg correlation key — never CSeq, which the B2BUA rewrites. */
export const typeKey = (m: Flows.Msg): string =>
  m.summary.kind === "request"
    ? `req:${m.summary.method.toUpperCase()}`
    : `resp:${m.summary.status}:${m.summary.cseq.method.toUpperCase()}`

/** The `[branch, position]` a `called[b][s]` position token names. */
export const parseCalledPos = (pos: string): readonly [number, number] | undefined => {
  const m = /^called\[(\d+)\]\[(\d+)\]$/.exec(pos)
  return m ? ([Number(m[1]), Number(m[2])] as const) : undefined
}

export interface BuiltMsg {
  readonly spec: MsgSpecDraft
  readonly resources: ReadonlyArray<ResourceFile>
  readonly flags: ReadonlyArray<Case.Flag>
}

/**
 * The message spec for one captured datagram at one vantage.
 *
 * `bodyStorable` is false on the automatic classes whose body the stack has
 * nowhere to put (§6.3) — a 100 Trying, an ACK to a non-2xx. The captured
 * payload is then DROPPED with a flag rather than stored where nothing would
 * emit it; the expect side still states its body, which asserts what arrived
 * and composes nothing.
 */
export const buildMsg = (
  flows: Flows.FlowsDoc,
  layout: Layout,
  plan: Plan,
  actor: ActorObs,
  msg: Flows.Msg,
  emits: boolean,
  slugText: string,
  parts: PartsIndex = new Map(),
  bodyStorable: boolean = true
): BuiltMsg => {
  const spec: MsgSpecDraft = {}
  const resources: Array<ResourceFile> = []
  const flags: Array<Case.Flag> = []

  if (msg.summary.kind === "request") {
    spec.method = msg.summary.method
    // Identity refs only on the dialog-OPENING INVITE: an in-dialog R-URI is
    // the tier-1 learned remote target, regenerated at replay.
    if (emits && Flows.isInvite(msg) && isInitialInvite(flows, actor, msg)) {
      spec.ruri = refFlagged(layout, plan, msg.summary.uri, "R-URI", flags)
      spec.from = refFlagged(layout, plan, msg.summary.from.uri, "From", flags)
      spec.to = refFlagged(layout, plan, msg.summary.to.uri, "To", flags)
    }
  } else {
    spec.status = msg.summary.status
    spec.reason = msg.summary.reason
    spec["cseq-method"] = msg.summary.cseq.method
  }

  const frozen: Array<Msg.Header> = []
  for (const h of headersInOrder(msg)) {
    if (OMIT_HEADERS.some((o) => sameHeader(o, h.name))) continue
    if (STACK_DERIVED_HEADERS.some((o) => sameHeader(o, h.name))) continue
    if (!emits && STACK_OWNED_EXPECT_HEADERS.some((o) => sameHeader(o, h.name))) continue
    if (!emits && SENDER_CLOCK_HEADERS.some((o) => sameHeader(o, h.name))) continue
    if (!emits && !carriesBody(msg) && BODY_DESCRIPTOR_HEADERS.some((o) => sameHeader(o, h.name))) continue
    const value = NUMBER_BEARING_HEADERS.some((n) => sameHeader(n, h.name))
      ? composeNumbers(layout, plan, h.value)
      : h.value
    frozen.push({ name: h.name, value })
  }
  if (frozen.length > 0) spec.headers = frozen

  if (emits && !bodyStorable) {
    if (carriesBody(msg)) {
      flags.push({
        kind: "automatic-body-dropped",
        detail:
          `${slugText} is a transaction-derived ${typeKey(msg)} whose captured body the stack ` +
          `has nowhere to place (§6.3), so it is not stored`
      })
    }
  } else if (emits) {
    const decomposed = decompose(msg, slugText, parts.get(msg))
    if (decomposed.undecomposed) {
      flags.push({
        kind: "multipart-body-unsupported",
        detail: `${slugText} carries a multipart body extraction did not decompose`
      })
    } else {
      if (decomposed.body !== undefined) spec.body = decomposed.body
      resources.push(...decomposed.resources)
      flags.push(...decomposed.flags)
    }
  } else {
    // Existence checks for stack-owned values, beside the frozen headers above:
    // a value the replayed stack mints per leg is checked for PRESENCE and
    // judged by the confrontation, never frozen. Existence carries no class, so
    // it gates on every lane — a header only a stack that runs the mechanism
    // emits belongs here only where the mechanism is MANDATORY (RFC 3262 §7.1:
    // a reliable provisional carries RSeq or it is not one). A session timer is
    // negotiated, not owed: a stack that runs none sends no Session-Expires,
    // which the confrontation judges as a delta and no step may gate on.
    const present: Array<string> = []
    if (STACK_OWNED_EXPECT_HEADERS.some((n) => hasHeader(msg, n))) present.push("rseq")
    if (STACK_DERIVED_HEADERS.some((n) => hasHeader(msg, n))) present.push("rack")
    if (present.length > 0) spec["headers-present"] = present
    const expected = expectBody(msg, slugText)
    if (expected.body !== undefined) spec.body = expected.body
    resources.push(...expected.resources)
    flags.push(...expected.flags)
  }

  return { spec, resources, flags }
}

/**
 * Tier-3 headers whose values carry numbers the plan may own (§8.1). Frozen,
 * they strand the document on the lane it was cut from, so a recognized number
 * is COMPOSED as `${num:<identity>:<form>}` and bound per lane instead.
 */
const NUMBER_BEARING_HEADERS = [
  "P-Asserted-Identity",
  "P-Preferred-Identity",
  "Remote-Party-ID",
  "Diversion",
  "History-Info",
  "Refer-To",
  "Referred-By"
]

const URI_RUN = /(?:sips?|tel):[^>\s,;]+/gi

/** The identity-registry name of a tier-2 position (§8.5's one encoding). */
const identityOf = (pos: string): string | undefined => {
  if (pos === "caller") return CALLER_IDENTITY
  const bs = parseCalledPos(pos)
  return bs ? calleeIdentity(bs[0], bs[1]) : undefined
}

/**
 * A number-bearing header value with every number the plan resolved to a
 * registered identity composed as a `${num:…}` accessor. A number the plan
 * does not classify, that maps to no topology position, or whose dial form
 * carries no label stays frozen: an accessor lint would refuse is worse than a
 * literal.
 */
const composeNumbers = (layout: Layout, plan: Plan, value: string): string => {
  let out = value
  for (const uri of value.match(URI_RUN) ?? []) {
    const user = uriUser(uri)
    if (user === "" || user.startsWith("urn:")) continue
    const cls = plan.classify(user)
    if (!cls) continue
    const pos = layout.posByKey.get(classKey(cls))
    if (pos === undefined) continue
    const name = identityOf(pos)
    const form = plan.formLabel(user)
    if (name === undefined || form === undefined) continue
    out = out.split(user).join(`\${num:${name}:${form}}`)
  }
  return out
}

const isInitialInvite = (flows: Flows.FlowsDoc, actor: ActorObs, msg: Flows.Msg): boolean => {
  if (actor.kind !== "uac") return false
  const first = actor.msgIdxs
    .map((i) => flows.legs[actor.origLeg]!.msgs[i]!)
    .filter((m) => peerSide(actor, m.src))
    .find(Flows.isInvite)
  return first === msg
}

/**
 * `resolveRef`, raising a flag when it must FREEZE a number-bearing ref: a
 * frozen routing number is unreplayable on a real lane, so the deferral lives in
 * the artifact rather than in a silent freeze.
 */
const refFlagged = (
  layout: Layout,
  plan: Plan,
  uri: string,
  field: string,
  flags: Array<Case.Flag>
): Msg.Ref => {
  const r = resolveRef(layout, plan, uri)
  if (Msg.isFrozenRef(r) && r.kind === undefined && hasNumberRun(r.frozen)) {
    flags.push({
      kind: "number-bearing-frozen-ref",
      detail: `${field} "${r.frozen}" embeds a number the plan does not recognize post-anonymization; frozen (not role-mapped) — declare its composed form in the network plan before a real lane replays it, or the ruri-pos claim will not match`
    })
  }
  return r
}

/** A frozen value carries a number if it has a digit run of 5 or more. */
const hasNumberRun = (s: string): boolean => /\d{5}/.test(s)

/**
 * RFC 3323 §4.1.1.3's own anonymous address, and what a ref falls back to when
 * the captured URI states no address at all: a frozen value must always compose
 * to a VALID URI, and an empty one cannot.
 */
const ANONYMOUS_ADDR = "anonymous@anonymous.invalid"

const SIP_URI = /^sips?:/i

const resolveRef = (layout: Layout, plan: Plan, uri: string): Msg.Ref => {
  const user = uriUser(uri)
  if (user.startsWith("urn:")) return { frozen: user, kind: "urn-service" }
  // An identity with no number to map is frozen as its whole ADDRESS, scheme
  // aside, and `kind` is what tells the lane it is an address and not a
  // userpart to host. A host-only URI (`sip:172.31.16.99`, RFC 3261 §19.1.1
  // userinfo being optional) has an EMPTY userpart and its host is the only
  // thing there is to freeze.
  if (user.toLowerCase() === "anonymous") return { frozen: addrForm(uri), kind: "anonymous" }
  if (user === "") {
    const addr = SIP_URI.test(uri.trim()) ? addrForm(uri) : ""
    return { frozen: addr === "" ? ANONYMOUS_ADDR : addr, kind: "anonymous" }
  }
  const cls = plan.classify(user)
  if (!cls) return { frozen: user }
  const pos = layout.posByKey.get(cls.kind === "nsn" ? cls.nsn : cls.digits)
  if (pos === undefined) return { frozen: user }
  const form = plan.formLabel(user)
  return form === undefined ? { pos } : { pos, form }
}
