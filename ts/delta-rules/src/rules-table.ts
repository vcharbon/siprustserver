/**
 * A deployment's rules table, rendered from its rules, its lane lists, its
 * overlap golden file and its refusal roster: the one Markdown page that
 * answers "what rules are there" without reading code. The deployment commits
 * the page and byte-checks it against this rendering, so it never drifts from
 * the rules it documents.
 *
 * Pure: the deployment hands in every input, its prose included.
 */
import { Policy } from "@sip/pipeline"
import type { LanePolicy } from "./lane-lists.js"
import type { GoldenPair } from "./overlap.js"
import type { DeltaRule } from "./rule.js"
import { kindsOf } from "./rule-on.js"

/** One refusal row of the roster, whatever its tier. */
export interface RefusalRow {
  readonly id: string
  readonly subject: string
  readonly disposition: string
  readonly why: string
}

/** Everything one rendering reads. */
export interface RulesTable<F, S extends string> {
  /** The deployment's rules, in match order. */
  readonly rules: ReadonlyArray<DeltaRule<F, S>>
  /** The sections in match order, each with the heading its table carries. */
  readonly sections: ReadonlyArray<{ readonly name: S; readonly title: string }>
  /** The lanes a column each, in column order. */
  readonly lanes: ReadonlyArray<string>
  /** Each lane's resolved lists; a lane without one shows `—`. */
  readonly policies: ReadonlyMap<string, LanePolicy>
  /** The pairs of the deployment's overlap golden file. */
  readonly overlap: ReadonlyArray<Pick<GoldenPair, "winner" | "shadowed">>
  /** The lines that open the page, ahead of the first section's table. */
  readonly intro: ReadonlyArray<string>
  /** The paragraph that opens the refusal table. */
  readonly refusalIntro: string
  /** The refusal rows by tier, in the order a capture meets them. */
  readonly refusals: ReadonlyArray<readonly [string, ReadonlyArray<RefusalRow>]>
}

/** For each rule, the rules it wins over and the rules it loses to, each in match order. */
interface Standing {
  readonly winsOver: ReadonlyArray<string>
  readonly losesTo: ReadonlyArray<string>
}

const standings = (
  rules: ReadonlyArray<{ readonly id: string }>,
  pairs: ReadonlyArray<Pick<GoldenPair, "winner" | "shadowed">>
): ReadonlyMap<string, Standing> => {
  const position = new Map(rules.map((rule, i) => [rule.id, i]))
  const byPosition = (ids: ReadonlySet<string>): ReadonlyArray<string> =>
    [...ids].sort((a, b) => (position.get(a) ?? Infinity) - (position.get(b) ?? Infinity) || a.localeCompare(b))
  const wins = new Map<string, Set<string>>()
  const loses = new Map<string, Set<string>>()
  const add = (map: Map<string, Set<string>>, key: string, value: string) =>
    map.set(key, (map.get(key) ?? new Set()).add(value))
  for (const pair of pairs) {
    add(wins, pair.winner, pair.shadowed)
    add(loses, pair.shadowed, pair.winner)
  }
  return new Map(rules.map((rule) => [rule.id, {
    winsOver: byPosition(wins.get(rule.id) ?? new Set()),
    losesTo: byPosition(loses.get(rule.id) ?? new Set())
  }]))
}

const ids = (list: ReadonlyArray<string>): string => list.map((id) => `\`${id}\``).join(", ")

const classOf = (policy: LanePolicy | undefined, id: string): string => {
  if (policy === undefined) return "—"
  if (policy.accepted.has(id)) return "accepted"
  const bug = policy.knownBug.get(id)
  if (bug !== undefined) return `known-bug (${bug.ticket ?? ""})`
  return "unlisted"
}

const cell = (text: string): string => text.replace(/\|/g, "\\|").replace(/\n/g, " ")

const ruleRow = <F>(rule: DeltaRule<F>, lanes: ReadonlyArray<string>, policies: ReadonlyMap<string, LanePolicy>): string => {
  const classes = lanes.map((lane) => classOf(policies.get(lane), rule.id))
  const gate = rule.gate === undefined ? "" : `\`${rule.gate}\``
  return `| \`${rule.id}\` | ${kindsOf(rule.on).join(", ")} | ${gate} | ${cell(rule.rfc ?? "")} | ${classes.join(" | ")} |`
}

const ruleNotes = <F>(rule: DeltaRule<F>, standing: Standing | undefined): ReadonlyArray<string> => {
  const out = [`- **\`${rule.id}\`** — ${rule.why}`]
  const on = [
    ...(rule.on.family === undefined ? [] : [`family ${rule.on.family.join(", ")}`]),
    ...(rule.on.scope === undefined ? [] : [`scope ${rule.on.scope.join(", ")}`])
  ]
  if (on.length > 0) out.push(`  - *on*: ${on.join("; ")}`)
  for (const [lane, note] of Object.entries(rule.lanes ?? {})) out.push(`  - *${lane}*: ${note}`)
  if (standing !== undefined && standing.winsOver.length > 0) out.push(`  - *wins over*: ${ids(standing.winsOver)}`)
  if (standing !== undefined && standing.losesTo.length > 0) out.push(`  - *loses to*: ${ids(standing.losesTo)}`)
  return out
}

/** The refusal tokens the pipeline states on its own account, whatever the deployment. */
const pipelineOwn: ReadonlySet<string> = new Set(Policy.refusalRoster(Policy.neutralPolicy).map((rule) => rule.id))

/** The whole page: the intro, one table and its notes per section that holds a rule, then the refusal table. */
export const render = <F, S extends string>(table: RulesTable<F, S>): string => {
  const lines: Array<string> = [...table.intro]
  const byRule = standings(table.rules, table.overlap)
  for (const { name, title } of table.sections) {
    const rules = table.rules.filter((r) => r.section === name)
    if (rules.length === 0) continue
    lines.push(`### ${title}`, "")
    lines.push(`| rule | kind | gate | rfc | ${table.lanes.join(" | ")} |`)
    lines.push(`|---|---|---|---|${table.lanes.map(() => "---").join("|")}|`)
    for (const rule of rules) lines.push(ruleRow(rule, table.lanes, table.policies))
    lines.push("")
    for (const rule of rules) lines.push(...ruleNotes(rule, byRule.get(rule.id)))
    lines.push("")
  }
  lines.push("## Refusal rules, in the order a capture meets them", "")
  lines.push(table.refusalIntro, "")
  lines.push("| tier | rule | home | subject | disposition | why |", "|---|---|---|---|---|---|")
  for (const [tier, rules] of table.refusals) {
    for (const rule of rules) {
      const home = pipelineOwn.has(rule.id) ? "pipeline" : "deployment"
      lines.push(`| ${tier} | \`${rule.id}\` | ${home} | ${rule.subject} | ${rule.disposition} | ${rule.why} |`)
    }
  }
  lines.push("")
  return lines.join("\n")
}
