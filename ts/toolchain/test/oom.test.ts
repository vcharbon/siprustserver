/**
 * OOM evidence off cgroup v2: which cgroup the process is in, how many OOM
 * kills its `memory.events` counts, and the words a failure line carries.
 */
import { describe, expect, it } from "vitest"
import { Oom } from "../src/oom.js"

describe("Oom", () => {
  it("reads the unified cgroup line, and none where the process sits in v1 only", () => {
    expect(Oom.unifiedPath("12:pids:/a\n0::/user.slice/run.scope\n")).toBe("/user.slice/run.scope")
    expect(Oom.unifiedPath("0::/\n")).toBe("/")
    expect(Oom.unifiedPath("12:memory:/a\n")).toBeUndefined()
  })

  it("reads the oom_kill count, never the oom count beside it", () => {
    expect(Oom.oomKillsOf("low 0\nhigh 0\nmax 3\noom 2\noom_kill 1\noom_group_kill 0\n")).toBe(1)
    expect(Oom.oomKillsOf("low 0\noom 2\n")).toBeUndefined()
  })

  it("counts the kills between the two reads, none where the count did not grow, and refuses to guess", () => {
    const at = (kills: number, cgroup = "/run.scope"): Oom.Count => ({ _tag: "Read", cgroup, kills })
    expect(Oom.evidenceOf(at(2), at(3), "1G")).toEqual({ _tag: "Recorded", cgroup: "/run.scope", kills: 1, memoryMax: "1G" })
    expect(Oom.evidenceOf(at(2), at(2), "1G")).toEqual({ _tag: "None", cgroup: "/run.scope" })
    const unreadable: Oom.Count = { _tag: "Unknown", why: "/proc/self/cgroup does not read" }
    expect(Oom.evidenceOf(unreadable, at(3), "1G")).toEqual(unreadable)
    expect(Oom.evidenceOf(at(2), unreadable, "1G")).toEqual(unreadable)
    expect(Oom.evidenceOf(at(2), at(9, "/other.scope"), "1G")).toEqual({
      _tag: "Unknown",
      why: "the process moved from cgroup /run.scope to /other.scope"
    })
  })

  it("words a recorded kill, an absent one and an unreadable cgroup apart", () => {
    expect(Oom.words({ _tag: "Recorded", cgroup: "/run.scope", kills: 1, memoryMax: "1073741824" }))
      .toBe("cgroup /run.scope recorded 1 OOM kill(s) while it ran (memory.max 1073741824)")
    expect(Oom.words({ _tag: "None", cgroup: "/run.scope" })).toBe("cgroup /run.scope recorded no OOM kill while it ran")
    expect(Oom.words({ _tag: "Unknown", why: "no cgroup v2 hierarchy holds this process" }))
      .toBe("OOM kills not readable: no cgroup v2 hierarchy holds this process")
  })
})
