/**
 * A cell digest as Markdown: the outcome first, then the gates, the
 * failures, the findings each with its message, the facts, the exchanges, the
 * SUT's records, the call's logs labelled by source, and the links.
 */
import type { Digest } from "./cell-digest.ts"

const text = (value: unknown): string => typeof value === "string" ? value : JSON.stringify(value)

/** The digest as Markdown. */
export const digestToMarkdown = (digest: Digest): string => {
  const out: string[] = [
    `# Cell ${digest.cell}`,
    "",
    `run ${digest.run} · capture ${digest.capture}${digest.lane === undefined ? "" : ` · lane ${digest.lane}`}`,
    ...(digest.sut === undefined ? [] : [`SUT on this lane: ${digest.sut}`]),
    "",
    `**${digest.outcome}**`,
    "",
    "## Gates",
    "",
    ...digest.gates.map((g) => `- ${g.gate}: ${g.passed ? "passed" : "FAILED"} · ${g.line}`)
  ]
  if (digest.deltaRows.length > 0) {
    out.push(
      "",
      "## Deltas no rule accepts",
      "",
      ...digest.deltaRows.map((r) => `- ${r.klass} · ${r.signature} · ${r.rows} rows · at ${r.steps.join(", ")}`)
    )
  }
  if (digest.failures.length > 0) out.push("", "## Verdict failures", "", ...digest.failures.map((f) => `- ${f}`))
  if (digest.findings.length > 0) {
    out.push("", "## RFC findings", "")
    for (const f of digest.findings) {
      out.push(`- ${f.standing} · ${f.rule} · charged ${f.charged}${f.at === undefined ? "" : ` · at ${f.at.line}`}`)
      out.push(`    ${f.detail}`)
      if (f.at !== undefined) out.push(`    message: ${f.at.href}`)
      if (f.note !== undefined) out.push(`    ${f.note}`)
    }
  }
  const facts = Object.entries(digest.laneFacts)
  if (facts.length > 0) out.push("", "## Lane facts", "", ...facts.map(([k, v]) => `- ${k}: ${text(v)}`))
  if (digest.routing.length > 0) out.push("", "## Routing exchanges", "", ...digest.routing.map((r) => `- ${r}`))
  if (digest.records !== undefined) out.push("", "## SUT records", "", digest.records)
  for (const log of digest.logs) {
    const role = log.role === undefined ? "" : ` (${log.role})`
    out.push("", `## ${log.sut ? "SUT log" : "Log"} ${log.source}${role}`, "", log.how, "", "```", ...log.lines, "```")
  }
  out.push("", "## Links", "", ...digest.links.map((l) => `- ${l.label}: ${l.href}`))
  return `${out.join("\n")}\n`
}

