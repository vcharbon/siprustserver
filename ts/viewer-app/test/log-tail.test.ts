/**
 * The tail's folding: each answer appends and moves the offset, a done
 * answer ends it, an answer from before what is held starts the text over,
 * the cap drops whole lines off the front and counts them, and a big chunk
 * asks for the next poll at once.
 */
import { describe, expect, it } from "vitest"
import { BIG_CHUNK, EMPTY, fold, nextDelay } from "../src/log-tail.ts"

describe("fold", () => {
  it("appends each answer and asks the next from where it ended", () => {
    const one = fold(EMPTY, { from: 0, to: 6, text: "first\n", done: false })
    expect(one).toEqual({ text: "first\n", from: 6, done: false, dropped: 0 })
    const two = fold(one, { from: 6, to: 13, text: "second\n", done: false })
    expect(two.text).toBe("first\nsecond\n")
    expect(two.from).toBe(13)
    const same = fold(two, { from: 13, to: 13, text: "", done: false })
    expect(same).toEqual(two)
  })

  it("ends on a done answer", () => {
    expect(fold(EMPTY, { from: 0, to: 4, text: "end\n", done: true }).done).toBe(true)
  })

  it("starts over when the answer begins before what is held", () => {
    const held = fold(EMPTY, { from: 0, to: 20, text: "twenty bytes of log\n", done: false })
    const replaced = fold(held, { from: 0, to: 4, text: "new\n", done: false })
    expect(replaced).toEqual({ text: "new\n", from: 4, done: false, dropped: 0 })
  })

  it("drops whole lines off the front past the cap and counts them", () => {
    const held = fold(EMPTY, { from: 0, to: 12, text: "aaaa\nbbbb\ncc", done: false }, 8)
    expect(held.text).toBe("bbbb\ncc")
    expect(held.dropped).toBe(5)
    // "bbbb\nccdddd\ne" over the cap by 5: the first line goes, and "ccdddd\ne" is exactly the cap.
    const more = fold(held, { from: 12, to: 18, text: "dddd\ne", done: false }, 8)
    expect(more.text).toBe("ccdddd\ne")
    expect(more.dropped).toBe(10)
    const again = fold(more, { from: 18, to: 20, text: "ff", done: false }, 8)
    expect(again.text).toBe("eff")
    expect(again.dropped).toBe(17)
  })
})

describe("nextDelay", () => {
  it("polls again at once after a big chunk, else at the cadence", () => {
    expect(nextDelay({ from: 0, to: BIG_CHUNK, text: "", done: false }, 1500)).toBe(0)
    expect(nextDelay({ from: 0, to: 40, text: "", done: false }, 1500)).toBe(1500)
  })
})
