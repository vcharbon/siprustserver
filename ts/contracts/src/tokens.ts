/**
 * The pivot's **string tokens**: closed vocabularies whose members carry a
 * parameter, and which ride the wire as one string (`pivot_schema::token`).
 *
 * Rust models each as an enum with `Display` + `FromStr`; TypeScript models each
 * as a pattern-constrained branded string plus a parse/build pair, so a consumer
 * that wants the structure asks for it and a consumer that only moves the value
 * around never re-parses it.
 */
import * as Schema from "effect/Schema"

// --- Lane verdict (`pivot_schema::case::LaneVerdict`) ------------------------

const LANE_VERDICT = /^(ok|blocked:.+)$/

/** A lane's replayability verdict: `ok`, or `blocked:<reason>` with an open reason token. */
export const LaneVerdict = Schema.String.pipe(
  Schema.check(Schema.isPattern(LANE_VERDICT, { title: "LaneVerdict" })),
  Schema.brand("LaneVerdict")
)
export type LaneVerdict = typeof LaneVerdict.Type

/** The verdict a token names. */
export type LaneVerdictValue = { readonly _tag: "ok" } | { readonly _tag: "blocked"; readonly reason: string }

export const parseLaneVerdict = (token: LaneVerdict | string): LaneVerdictValue => {
  if (token === "ok") return { _tag: "ok" }
  const reason = token.startsWith("blocked:") ? token.slice("blocked:".length) : ""
  if (reason === "") throw new Error(`lane verdict ${JSON.stringify(token)} is neither 'ok' nor 'blocked:<reason>'`)
  return { _tag: "blocked", reason }
}

export const laneVerdictToken = (value: LaneVerdictValue): LaneVerdict =>
  (value._tag === "ok" ? "ok" : `blocked:${value.reason}`) as LaneVerdict

/** Whether the lane may run the case. */
export const laneVerdictIsOk = (token: LaneVerdict | string): boolean => token === "ok"

// --- Attempt cause (`pivot_schema::call::Cause`) -----------------------------

const CAUSE = /^(no-answer|busy|transaction-timeout|closed:bye|redirect:3[0-9]{2}|external:[4-6][0-9]{2})$/

/**
 * Why the platform left an attempt. Closed: every member is read off a captured
 * datagram, and cites either the attempt's dialog-creating final or an actual
 * closer.
 */
export const Cause = Schema.String.pipe(
  Schema.check(Schema.isPattern(CAUSE, { title: "Cause" })),
  Schema.brand("Cause")
)
export type Cause = typeof Cause.Type

export type CauseValue =
  | { readonly _tag: "no-answer" }
  | { readonly _tag: "busy" }
  | { readonly _tag: "transaction-timeout" }
  | { readonly _tag: "closed-bye" }
  | { readonly _tag: "redirect"; readonly status: number }
  | { readonly _tag: "external"; readonly status: number }

export const parseCause = (token: Cause | string): CauseValue => {
  switch (token) {
    case "no-answer":
      return { _tag: "no-answer" }
    case "busy":
      return { _tag: "busy" }
    case "transaction-timeout":
      return { _tag: "transaction-timeout" }
    case "closed:bye":
      return { _tag: "closed-bye" }
    default:
      break
  }
  const redirect = token.startsWith("redirect:") ? Number(token.slice("redirect:".length)) : NaN
  if (redirect >= 300 && redirect <= 399) return { _tag: "redirect", status: redirect }
  const external = token.startsWith("external:") ? Number(token.slice("external:".length)) : NaN
  if (external >= 400 && external <= 699) return { _tag: "external", status: external }
  throw new Error(`cause ${JSON.stringify(token)} is not in the closed vocabulary`)
}

export const causeToken = (value: CauseValue): Cause => {
  switch (value._tag) {
    case "no-answer":
    case "busy":
    case "transaction-timeout":
      return value._tag as Cause
    case "closed-bye":
      return "closed:bye" as Cause
    case "redirect":
      return `redirect:${value.status}` as Cause
    case "external":
      return `external:${value.status}` as Cause
  }
}

/** Lowest and highest `no_answer_ms` a whole-second ring timer can have armed. */
export const NO_ANSWER_MS_BAND = { min: 1_000, max: 3_600_000 } as const

// --- Delay anchor (`pivot_schema::flow::Anchor`) -----------------------------

const ANCHOR = /^(trigger|step:[^ ]+)$/

/** What a dwell is measured from: the case's start, or a named step. */
export const Anchor = Schema.String.pipe(
  Schema.check(Schema.isPattern(ANCHOR, { title: "Anchor" })),
  Schema.brand("Anchor")
)
export type Anchor = typeof Anchor.Type

export type AnchorValue = { readonly _tag: "trigger" } | { readonly _tag: "step"; readonly step: string }

export const parseAnchor = (token: Anchor | string): AnchorValue => {
  if (token === "trigger") return { _tag: "trigger" }
  const step = token.startsWith("step:") ? token.slice("step:".length) : ""
  if (step === "") throw new Error(`delay anchor ${JSON.stringify(token)} is neither 'trigger' nor 'step:<id>'`)
  return { _tag: "step", step }
}

export const anchorToken = (value: AnchorValue): Anchor =>
  (value._tag === "trigger" ? "trigger" : `step:${value.step}`) as Anchor

/** The step id an anchor names, if it names one. */
export const anchorStep = (token: Anchor | string): string | undefined => {
  const value = parseAnchor(token)
  return value._tag === "step" ? value.step : undefined
}

// --- Accessor (`pivot_schema::accessor::Accessor`) ---------------------------

const ACCESSOR = /^\$\{((leg|early|step):[^.{}]+\.[^{}]+|num:[^:{}]+:[^:{}]+)\}$/

/**
 * One run-time value: `${leg:<id>.<field>}`, `${early:<id>.<field>}`,
 * `${step:<id>.<field>}` or `${num:<identity>:<form>}`.
 */
export const Accessor = Schema.String.pipe(
  Schema.check(Schema.isPattern(ACCESSOR, { title: "Accessor" })),
  Schema.brand("Accessor")
)
export type Accessor = typeof Accessor.Type

/** What a leg accessor reads off runner dialog state. */
export const LEG_FIELDS = [
  "call-id",
  "local-tag",
  "remote-tag",
  "remote-target",
  "route-set",
  "cseq.local",
  "cseq.remote",
  "rseq"
] as const
export type LegField = (typeof LEG_FIELDS)[number]

/** What an early-dialog accessor reads off ONE fork of a forking leg. */
export const EARLY_FIELDS = ["tag", "rseq"] as const
export type EarlyField = (typeof EARLY_FIELDS)[number]

/** What a step accessor reads off a message the run already handled. */
export type StepField =
  | { readonly _tag: "header"; readonly name: string }
  | { readonly _tag: "cseq" }
  | { readonly _tag: "rseq" }
  | { readonly _tag: "status" }
  | { readonly _tag: "branch" }

export type AccessorValue =
  | { readonly _tag: "leg"; readonly leg: string; readonly field: LegField }
  | { readonly _tag: "early"; readonly early: string; readonly field: EarlyField }
  | { readonly _tag: "step"; readonly step: string; readonly field: StepField }
  | { readonly _tag: "num"; readonly name: string; readonly form: string }

/** The id an accessor targets — a leg id, a step id or an identity name. */
export const accessorTarget = (value: AccessorValue): string => {
  switch (value._tag) {
    case "leg":
      return value.leg
    case "early":
      return value.early
    case "step":
      return value.step
    case "num":
      return value.name
  }
}

/** Parse the INSIDE of a `${…}`, or say why it is not an accessor. */
export const parseAccessorBody = (body: string): AccessorValue => {
  const colon = body.indexOf(":")
  if (colon < 0) {
    throw new Error(`accessor ${JSON.stringify(body)} states no namespace (\`leg:\`, \`early:\`, \`step:\` or \`num:\`)`)
  }
  const namespace = body.slice(0, colon)
  const rest = body.slice(colon + 1)
  if (namespace === "num") {
    const sep = rest.indexOf(":")
    if (sep < 0) throw new Error(`accessor ${JSON.stringify(body)} names an identity but no dial form`)
    const name = rest.slice(0, sep)
    const form = rest.slice(sep + 1)
    if (name === "" || form === "") {
      throw new Error(`accessor ${JSON.stringify(body)} states an empty identity name or dial form`)
    }
    return { _tag: "num", name, form }
  }
  const dot = rest.indexOf(".")
  if (dot < 0) throw new Error(`accessor ${JSON.stringify(body)} states an id but no field`)
  const id = rest.slice(0, dot)
  const field = rest.slice(dot + 1)
  if (id === "") throw new Error(`accessor ${JSON.stringify(body)} names no ${namespace}`)
  switch (namespace) {
    case "leg": {
      const found = LEG_FIELDS.find((f) => f === field)
      if (found === undefined) {
        throw new Error(`leg field ${JSON.stringify(field)} is none of ${LEG_FIELDS.join(", ")}`)
      }
      return { _tag: "leg", leg: id, field: found }
    }
    case "early": {
      const found = EARLY_FIELDS.find((f) => f === field)
      if (found === undefined) {
        throw new Error(`early-dialog field ${JSON.stringify(field)} is none of ${EARLY_FIELDS.join(", ")}`)
      }
      return { _tag: "early", early: id, field: found }
    }
    case "step":
      return { _tag: "step", step: id, field: parseStepField(field) }
    default:
      throw new Error(
        `accessor namespace ${JSON.stringify(namespace)} is none of \`leg\`, \`early\`, \`step\`, \`num\``
      )
  }
}

const parseStepField = (field: string): StepField => {
  switch (field) {
    case "cseq":
      return { _tag: "cseq" }
    case "rseq":
      return { _tag: "rseq" }
    case "status":
      return { _tag: "status" }
    case "branch":
      return { _tag: "branch" }
    default:
      break
  }
  if (field === "header.") throw new Error("accessor `header.` names no header")
  if (field.startsWith("header.")) return { _tag: "header", name: field.slice("header.".length) }
  throw new Error(`step field ${JSON.stringify(field)} is none of header.<name>, cseq, rseq, status, branch`)
}

export const parseAccessor = (token: Accessor | string): AccessorValue => {
  if (!token.startsWith("${") || !token.endsWith("}")) {
    throw new Error(`accessor ${JSON.stringify(token)} is not wrapped in \`\${…}\``)
  }
  return parseAccessorBody(token.slice(2, -1))
}

export const accessorToken = (value: AccessorValue): Accessor => {
  switch (value._tag) {
    case "leg":
      return `\${leg:${value.leg}.${value.field}}` as Accessor
    case "early":
      return `\${early:${value.early}.${value.field}}` as Accessor
    case "step":
      return `\${step:${value.step}.${stepFieldToken(value.field)}}` as Accessor
    case "num":
      return `\${num:${value.name}:${value.form}}` as Accessor
  }
}

const stepFieldToken = (field: StepField): string =>
  field._tag === "header" ? `header.${field.name}` : field._tag

/**
 * Every `${…}` in `text`, each parsed or refused. An unterminated `${` is itself
 * a refusal, so a truncated accessor cannot pass as literal text.
 */
export const scanAccessors = (text: string): Array<AccessorValue | Error> => {
  const out: Array<AccessorValue | Error> = []
  let rest = text
  for (;;) {
    const start = rest.indexOf("${")
    if (start < 0) return out
    const after = rest.slice(start + 2)
    const end = after.indexOf("}")
    if (end < 0) {
      out.push(new Error(`accessor ${JSON.stringify(rest.slice(start))} is not terminated by \`}\``))
      return out
    }
    try {
      out.push(parseAccessorBody(after.slice(0, end)))
    } catch (cause) {
      out.push(cause instanceof Error ? cause : new Error(String(cause)))
    }
    rest = after.slice(end + 1)
  }
}

/** Whether `text` carries any `${…}` at all — what the generator-subset gate tests. */
export const accessorPresentIn = (text: string): boolean => text.includes("${")

/**
 * The one computable form: a referenced value plus a signed offset. There is no
 * expression language — `{ from, delta }` covers "the CSeq the peer sent, plus
 * one", which is every arithmetic the corpus needs.
 */
export const Computed = Schema.Struct({
  from: Accessor,
  delta: Schema.Int
})
export interface Computed extends Schema.Schema.Type<typeof Computed> {}

// --- Position (`pivot_schema::call::Position`) -------------------------------

export type PositionRole =
  | { readonly _tag: "caller" }
  | { readonly _tag: "called"; readonly branch: number; readonly position: number }

/**
 * A tier-2 position token: `caller` or `called[branch][position]`, optionally
 * qualified by a call id — required once a document declares more than one call.
 */
export interface Position {
  readonly call?: string
  readonly role: PositionRole
}

export const parsePosition = (text: string): Position => {
  const dot = text.indexOf(".")
  const call = dot > 0 ? text.slice(0, dot) : undefined
  const rest = dot > 0 ? text.slice(dot + 1) : text
  if (rest === "caller") return { call, role: { _tag: "caller" } }
  const match = /^called\[(\d+)]\[(\d+)]$/.exec(rest)
  if (match === null) {
    throw new Error(`position ${JSON.stringify(text)} is neither \`caller\` nor \`called[b][s]\`, qualified or bare`)
  }
  return { call, role: { _tag: "called", branch: Number(match[1]), position: Number(match[2]) } }
}

export const positionToken = (position: Position): string => {
  const prefix = position.call === undefined ? "" : `${position.call}.`
  const role = position.role
  return role._tag === "caller" ? `${prefix}caller` : `${prefix}called[${role.branch}][${role.position}]`
}
