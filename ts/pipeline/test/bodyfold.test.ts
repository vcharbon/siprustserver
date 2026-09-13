import { describe, expect, it } from "vitest"
import { bodiesEqual, foldBody } from "../src/bodyfold.js"

describe("foldBody", () => {
  const declared = '<?xml version="1.0" encoding="utf-8"?>\r\n<a x="1" y="2">\r\n  <b>t</b>\r\n</a>\r\n'

  it("`exact` is identity, and the default", () => {
    expect(foldBody("exact", declared)).toBe(declared)
    expect(foldBody(undefined, declared)).toBe(declared)
  })

  it("`xml` drops the declaration and the whitespace between tags, and trims", () => {
    expect(foldBody("xml", declared)).toBe('<a x="1" y="2"><b>t</b></a>')
    expect(bodiesEqual("xml", declared, '<a x="1" y="2"><b>t</b></a>')).toBe(true)
  })

  it("`xml` erases nothing else: attribute order, text whitespace, comments and entities all count", () => {
    expect(bodiesEqual("xml", '<a x="1" y="2"/>', '<a y="2" x="1"/>')).toBe(false)
    expect(bodiesEqual("xml", "<b>t u</b>", "<b>t  u</b>")).toBe(false)
    expect(bodiesEqual("xml", "<a><!-- c --></a>", "<a></a>")).toBe(false)
    expect(bodiesEqual("xml", "<b>&amp;</b>", "<b>&#38;</b>")).toBe(false)
    expect(bodiesEqual("xml", "<a/>", "<a></a>")).toBe(false)
  })
})
