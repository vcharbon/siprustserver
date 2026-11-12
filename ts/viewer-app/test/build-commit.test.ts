/**
 * A build stamps the commit the builder states, else the short commit of the
 * outermost checkout holding the app (its superproject's where it is a
 * submodule), `-dirty` where that tree differs from it, and no commit outside
 * any checkout.
 */
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { commitOf, COMMIT_VARIABLE, stampOf } from "../build-commit.ts"

const dirs: Array<string> = []
afterAll(() => {
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true })
})

/** A fresh directory, removed once the file's tests end. */
const scratch = (): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "build-commit-"))
  dirs.push(dir)
  return dir
}

/** The environment a git under test runs in: no inherited repository, a fixed author. */
const GIT_ENV: NodeJS.ProcessEnv = {
  PATH: process.env["PATH"],
  HOME: os.tmpdir(),
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@example.invalid",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@example.invalid"
}

const git = (dir: string, ...args: Array<string>): string =>
  execFileSync("git", ["-C", dir, "-c", "protocol.file.allow=always", ...args], { encoding: "utf8", env: GIT_ENV })
    .trim()

/** A repository at `dir` holding one committed file. */
const repoAt = (dir: string): string => {
  fs.mkdirSync(dir, { recursive: true })
  git(dir, "init", "-q", "-b", "main")
  fs.writeFileSync(path.join(dir, "file.txt"), "one\n")
  git(dir, "add", "file.txt")
  git(dir, "commit", "-q", "-m", "one")
  return dir
}

describe("the commit a build stamps", () => {
  it("is the checkout's short commit, the app directory anywhere under it", () => {
    const repo = repoAt(scratch())
    const app = path.join(repo, "app")
    fs.mkdirSync(app)
    expect(commitOf(app, {})).toBe(git(repo, "rev-parse", "--short", "HEAD"))
  })

  it("is marked dirty where the tree differs from the commit", () => {
    const repo = repoAt(scratch())
    fs.writeFileSync(path.join(repo, "file.txt"), "two\n")
    expect(commitOf(repo, {})).toBe(`${git(repo, "rev-parse", "--short", "HEAD")}-dirty`)
  })

  it("is the superproject's where the app lies in a submodule", () => {
    const root = scratch()
    const inner = repoAt(path.join(root, "inner"))
    const outer = repoAt(path.join(root, "outer"))
    git(outer, "submodule", "add", "-q", inner, "sub")
    git(outer, "commit", "-q", "-m", "sub")
    const sub = path.join(outer, "sub")
    expect(git(sub, "rev-parse", "--short", "HEAD")).not.toBe(git(outer, "rev-parse", "--short", "HEAD"))
    expect(commitOf(sub, {})).toBe(git(outer, "rev-parse", "--short", "HEAD"))
  })

  it("is the one the builder states, whatever the checkout", () => {
    const repo = repoAt(scratch())
    expect(commitOf(repo, { [COMMIT_VARIABLE]: "stated1" })).toBe("stated1")
    expect(commitOf(scratch(), { [COMMIT_VARIABLE]: "stated2" })).toBe("stated2")
  })

  it("is none outside any checkout", () => {
    const outside = scratch()
    expect(commitOf(outside, {})).toBeNull()
    expect(stampOf(outside, {}, new Date("2026-11-12T18:04:00.000Z"))).toEqual({
      commit: null,
      builtAt: "2026-11-12T18:04:00.000Z"
    })
  })
})
