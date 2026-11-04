/**
 * A check card reads two values of one thing character by character, and a
 * statement the seen broke plain, with no empty said line.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { CampaignCheck } from "../src/api.ts"
import { CheckCard, comparesCharacters } from "../src/components/CheckCard.tsx"

const value: CampaignCheck = {
  anchor: "C1 INVITE",
  field: "header(Content-Type)",
  expected: "application/sdp",
  seen: "application/isup",
  said: null,
  side: "sut",
  compare: "value"
}

const statement: CampaignCheck = {
  anchor: "post-run cleanup",
  field: "sut.reaped",
  expected: "every call removed, no lock left",
  seen: "still held 40000 ms after the flow ended",
  said: null,
  side: "sut",
  compare: "statement"
}

const render = (check: CampaignCheck): string =>
  renderToStaticMarkup(<CheckCard check={check} sides={["decision", "sut"]} saidLabel="decision said" />)

describe("check card", () => {
  it("compares two values of one thing character by character", () => {
    expect(comparesCharacters(value)).toBe(true)
    expect(render(value)).toContain("<del>")
  })

  it("reads a statement plain, and drops an empty said line", () => {
    expect(comparesCharacters(statement)).toBe(false)
    const html = render(statement)
    expect(html).not.toContain("<del>")
    expect(html).not.toContain("<ins>")
    expect(html).toContain("still held 40000 ms after the flow ended")
    expect(html).not.toContain("decision said")
  })

  it("keeps the said line of a value check, empty or not", () => {
    expect(render(value)).toContain("decision said")
  })

  it("links its anchor to the message it was made at, where the host shows one, and reads it plain otherwise", () => {
    const linked = renderToStaticMarkup(
      <CheckCard check={value} sides={["sut"]} saidLabel="said" onAnchor={() => undefined} />
    )
    expect(linked).toMatch(/<button type="button" class="cp-anchor"[^>]*>C1 INVITE →<\/button> · header\(Content-Type\)/)
    expect(render(value)).not.toContain("cp-anchor")
    expect(render(value)).toContain("✗ C1 INVITE · header(Content-Type)")
  })
})
