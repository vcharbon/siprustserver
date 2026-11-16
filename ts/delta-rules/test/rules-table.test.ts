/**
 * The rules page: one table per section that holds a rule, each lane's class,
 * each rule's notes with its standing from the overlap pairs, and the refusal
 * table with the home of each refusal.
 */
import { Policy } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { render } from "../src/rules-table.js"
import { section } from "../src/rule.js"

type S = "first" | "second" | "empty"

const rules = [
  ...section<unknown, S>("first", [
    {
      id: "via-stack-owned",
      on: { kind: "header", family: ["via"] },
      rfc: "RFC 3261 §8.1.1.7",
      why: "The branch is the sending stack's own.",
      lanes: { lab: "kept apart" },
      header: () => true
    },
    { id: "any-header", on: { kind: "header" }, why: "Anything at all.", header: () => true }
  ]),
  ...section<unknown, S>("second", [
    { id: "a-retransmission", on: { kind: "shape", family: ["retransmission"] }, why: "A count.", shape: () => true }
  ])
]

const page = render({
  rules,
  sections: [
    { name: "first", title: "First" },
    { name: "empty", title: "Empty" },
    { name: "second", title: "Second" }
  ],
  lanes: ["lab", "bare"],
  policies: new Map([[
    "lab",
    { lane: "lab", accepted: new Map([["via-stack-owned", { rule: "via-stack-owned" }]]), knownBug: new Map([["any-header", { rule: "any-header", ticket: "T-1" }]]) }
  ]]),
  overlap: [{ winner: "via-stack-owned", shadowed: "any-header" }],
  intro: ["# Rules", ""],
  refusalIntro: "Refusals delete the case.",
  refusals: [["source", [{ id: "a-source-refusal", subject: "file", disposition: "refused", why: "unreadable" }]]]
})

describe("the rules page", () => {
  it("opens with the deployment's intro and prints a table per section that holds a rule", () => {
    const lines = page.split("\n")
    expect(lines.slice(0, 2)).toEqual(["# Rules", ""])
    expect(lines.filter((line) => line.startsWith("### "))).toEqual(["### First", "### Second"])
  })

  it("states each rule's kind, rfc and class on every lane, a lane with no list shown blank", () => {
    expect(page).toContain("| `via-stack-owned` | header |  | RFC 3261 §8.1.1.7 | accepted | — |")
    expect(page).toContain("| `any-header` | header |  |  | known-bug (T-1) | — |")
  })

  it("notes each rule's key, lane notes and standing", () => {
    expect(page).toContain(
      [
        "- **`via-stack-owned`** — The branch is the sending stack's own.",
        "  - *on*: family via",
        "  - *lab*: kept apart",
        "  - *wins over*: `any-header`"
      ].join("\n")
    )
    expect(page).toContain("- **`any-header`** — Anything at all.\n  - *loses to*: `via-stack-owned`")
  })

  it("closes with the refusal table, each refusal's home named", () => {
    const own = Policy.refusalRoster(Policy.neutralPolicy)[0]
    const withOwn = render({
      rules: [],
      sections: [],
      lanes: [],
      policies: new Map(),
      overlap: [],
      intro: [],
      refusalIntro: "Refusals.",
      refusals: [["case", [{ id: own!.id, subject: "s", disposition: "d", why: "w" }]]]
    })
    expect(page).toContain("| source | `a-source-refusal` | deployment | file | refused | unreadable |")
    expect(withOwn).toContain(`| case | \`${own!.id}\` | pipeline | s | d | w |`)
    expect(page.endsWith("|\n")).toBe(true)
  })
})
