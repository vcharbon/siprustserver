/**
 * A campaign start's pick of lanes: a refused lane is never picked, the
 * default pick is the first runnable lane's group, and a pick of two groups
 * is refused naming the groups that run.
 */
import { describe, expect, it } from "vitest"
import type { LaunchLane } from "../src/api.ts"
import { defaultPick, groups, groupsLine, pickRefusal } from "../src/campaign-pick.ts"

const lane = (id: string, group: string, refused: string | null = null): LaunchLane => ({ lane: id, group, refused })

const SPLIT = [lane("sim", "g1"), lane("lab-a", "g2"), lane("lab-b", "g2")]
const AWAY = [lane("sim", "g2"), lane("lab-a", "g2", "not here: real clock"), lane("lab-b", "g2", "not here: real clock")]

describe("a campaign start's lanes", () => {
  it("group the runnable lanes, in the order first met", () => {
    expect(groups(SPLIT)).toEqual([["sim"], ["lab-a", "lab-b"]])
    expect(groupsLine(SPLIT)).toBe("sim alone, or lab-a + lab-b")
    expect(groups(AWAY)).toEqual([["sim"]])
  })

  it("pick the first runnable lane's group by default, never a refused lane", () => {
    expect(defaultPick(SPLIT)).toEqual(["sim"])
    expect(defaultPick([lane("lab-a", "g2", "no"), lane("lab-b", "g2"), lane("sim", "g1")])).toEqual(["lab-b"])
    expect(defaultPick(AWAY)).toEqual(["sim"])
    expect(defaultPick([lane("lab-a", "g2", "no")])).toEqual([])
  })

  it("start a pick of one group of runnable lanes", () => {
    expect(pickRefusal(["lab-a", "lab-b"], SPLIT)).toBeNull()
    expect(pickRefusal(["sim"], AWAY)).toBeNull()
  })

  it("refuse a pick of two groups naming the groups that run, a refused lane with its reason, and no pick", () => {
    expect(pickRefusal(["sim", "lab-a"], SPLIT)).toBe("lanes of different groups run apart: sim alone, or lab-a + lab-b")
    expect(pickRefusal(["sim", "lab-a"], AWAY)).toBe("not here: real clock")
    expect(pickRefusal([], SPLIT)).toBe("no lane picked")
    expect(pickRefusal(["gone"], SPLIT)).toBe("no lane gone")
  })
})
