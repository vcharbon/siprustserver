/**
 * The rules page: one table per section that holds a rule, each lane's class,
 * each rule's notes with its standing from the overlap pairs, and the lines the
 * deployment closes it with.
 */
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
  trailing: ["## Elsewhere", "", "A table of the deployment's own.", ""]
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

  it("closes with the deployment's trailing lines after the last section's notes", () => {
    expect(page.endsWith("  - *on*: family retransmission\n\n## Elsewhere\n\nA table of the deployment's own.\n")).toBe(true)
  })

  it("closes on the last section's notes where the deployment adds nothing", () => {
    const bare = render({
      rules,
      sections: [{ name: "second", title: "Second" }],
      lanes: ["lab"],
      policies: new Map(),
      overlap: [],
      intro: [],
      trailing: []
    })
    expect(bare.split("\n")).toEqual([
      "### Second",
      "",
      "| rule | kind | gate | rfc | lab |",
      "|---|---|---|---|---|",
      "| `a-retransmission` | shape |  |  | — |",
      "",
      "- **`a-retransmission`** — A count.",
      "  - *on*: family retransmission",
      ""
    ])
  })
})
