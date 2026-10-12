/**
 * The scratch database name: one per checkout, so two worktrees running one
 * suite at once never build or drop each other's database; and a url pointed
 * at another database of the same server.
 */
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Client from "../src/client.js"

const nameOf = (base: string, cwd: string): Promise<string> =>
  Effect.runPromise(Client.scratchDatabase(base, cwd).pipe(Effect.provide(NodeServices.layer)))

const root = fs.mkdtempSync(path.join(os.tmpdir(), "scratch-db-"))
afterAll(() => fs.rmSync(root, { recursive: true, force: true }))

const checkout = (...parts: ReadonlyArray<string>): string => {
  const dir = path.join(root, ...parts)
  fs.mkdirSync(path.join(dir, "ts", "store"), { recursive: true })
  fs.writeFileSync(path.join(dir, ".git"), "gitdir: elsewhere\n")
  return dir
}

describe("Client.scratchDatabase", () => {
  it("suffixes the base with the checkout's directory name and a hash of its path, from anywhere under it", async () => {
    const dir = checkout("dev-12-Second.Final")
    const name = await nameOf("store_cut_test", path.join(dir, "ts/store"))
    expect(name).toMatch(/^store_cut_test_dev_12_second_final_[0-9a-f]{8}$/)
    expect(await nameOf("store_cut_test", dir)).toBe(name)
  })

  it("tells apart two checkouts whose directories share a name", async () => {
    const one = await nameOf("store_cut_test", checkout("a", "server"))
    const two = await nameOf("store_cut_test", checkout("b", "server"))
    expect(one).not.toBe(two)
    expect(one.startsWith("store_cut_test_server_")).toBe(true)
  })

  it("stays within postgres' 63 characters", async () => {
    const dir = checkout("a-very-long-checkout-directory-name-that-goes-on-and-on-and-on")
    const name = await nameOf("store_a_long_suite_name_test", dir)
    expect(name.length).toBeLessThanOrEqual(63)
    expect(name).toMatch(/_[0-9a-f]{8}$/)
  })

  it("falls back to the directory itself where no checkout is above it", async () => {
    const bare = path.join(root, "no-git-here")
    fs.mkdirSync(bare, { recursive: true })
    expect(await nameOf("store_cut_test", bare)).toMatch(/^store_cut_test_no_git_here_[0-9a-f]{8}$/)
  })
})

describe("Client.atDatabase", () => {
  it("replaces the database of a socket url and keeps its parameters", async () => {
    expect(Client.atDatabase("postgresql:///sip_store?host=/var/run/postgresql&port=5433", "other"))
      .toBe("postgresql:///other?host=/var/run/postgresql&port=5433")
  })
})
