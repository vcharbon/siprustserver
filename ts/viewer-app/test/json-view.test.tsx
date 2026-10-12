/**
 * How a JSON document is shown: a small one open in its `<pre>`, a large one
 * folded behind a bar that states its size and line count, with no `<pre>`
 * mounted until it is asked for. `describeJson` is the sizing both read.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import { JsonView } from "../src/components/JsonView.tsx"
import { choiceFor, describeJson, hasToggle, isOpen, JSON_FOLD_CHARS } from "../src/json-size.ts"

const small = { a: 1 }
/** A document well past the fold: a few hundred lines, several KB. */
const large = Array.from({ length: 300 }, (_, i) => `entry-${String(i).padStart(4, "0")}-xxxxxxxxxx`)

describe("describeJson", () => {
  it("gives the pretty text, its byte size and its line count", () => {
    const described = describeJson(small)
    expect(described.text).toBe("{\n  \"a\": 1\n}")
    expect(described.bytes).toBe(12)
    expect(described.lines).toBe(3)
  })

  it("counts bytes, not characters, so a multi-byte text is sized as sent", () => {
    expect(describeJson("é").bytes).toBe(4)
  })

  it("folds above the threshold and renders open below it", () => {
    expect(describeJson(small).folded).toBe(false)
    const described = describeJson(large)
    expect(described.text.length).toBeGreaterThan(JSON_FOLD_CHARS)
    expect(described.folded).toBe(true)
    expect(described.lines).toBe(302)
  })
})

describe("the fold state across documents", () => {
  const big = describeJson(large)
  const little = describeJson(small)

  it("opens unfolded and closed folded where the reader chose nothing", () => {
    expect(isOpen(little.folded, undefined)).toBe(true)
    expect(isOpen(big.folded, undefined)).toBe(false)
    expect(hasToggle(little.folded, undefined)).toBe(false)
    expect(hasToggle(big.folded, undefined)).toBe(true)
  })

  it("keeps the reader's choice for the text it was made on, and offers the toggle", () => {
    const hidden = { text: big.text, shown: false }
    expect(choiceFor(hidden, big.text)).toBe(false)
    expect(isOpen(big.folded, choiceFor(hidden, big.text))).toBe(false)
    expect(hasToggle(big.folded, choiceFor({ text: big.text, shown: true }, big.text))).toBe(true)
  })

  it("starts afresh on another document: a small one after a hidden large one opens, with no toggle", () => {
    const hidden = { text: big.text, shown: false }
    const shown = choiceFor(hidden, little.text)
    expect(shown).toBeUndefined()
    expect(isOpen(little.folded, shown)).toBe(true)
    expect(hasToggle(little.folded, shown)).toBe(false)
  })
})

describe("JsonView", () => {
  it("renders a small value open, its text in the pre", () => {
    const markup = renderToStaticMarkup(<JsonView value={small} label="case.json" />)
    expect(markup).toContain("case.json")
    expect(markup).toContain("<pre>")
    expect(markup).toContain("&quot;a&quot;: 1")
  })

  it("renders a large value folded: no pre, and the bar states the size and the line count", () => {
    const described = describeJson(large)
    const markup = renderToStaticMarkup(<JsonView value={large} label="pivot.json" />)
    expect(markup).toContain("pivot.json")
    expect(markup).not.toContain("<pre")
    expect(markup).toContain(`${described.lines} lines`)
    expect(markup).toContain("KB")
    expect(markup).toContain("show")
  })
})
