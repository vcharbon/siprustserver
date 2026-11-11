/**
 * The test server a run states: the exported `STORE_TEST_URL` over the
 * workspace's env file, the file's last line over its earlier ones, and
 * nothing where neither states one.
 */
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import * as ServerUrl from "./server-url.js"

const root = fs.mkdtempSync(path.join(os.tmpdir(), "server-url-"))
afterAll(() => fs.rmSync(root, { recursive: true, force: true }))

const envFile = (name: string, text: string): string => {
  const file = path.join(root, name)
  fs.writeFileSync(file, text)
  return file
}

const stated = (env: NodeJS.ProcessEnv, file: string): Promise<string | undefined> =>
  Effect.runPromise(ServerUrl.stated(env, file).pipe(Effect.provide(NodeServices.layer))).then(
    Option.getOrUndefined
  )

describe("ServerUrl.stated", () => {
  const file = envFile(
    "two-lines",
    "OTHER=x\nSTORE_TEST_URL=postgresql:///postgres?host=/first\nSTORE_TEST_URL=postgresql:///postgres?host=/run&port=5444\n"
  )

  it("is the exported url, over the env file's", async () => {
    expect(await stated({ STORE_TEST_URL: "postgresql:///postgres?host=/exported" }, file)).toBe(
      "postgresql:///postgres?host=/exported"
    )
  })

  it("is the env file's last line when none is exported, or the export is empty", async () => {
    expect(await stated({}, file)).toBe("postgresql:///postgres?host=/run&port=5444")
    expect(await stated({ STORE_TEST_URL: "" }, file)).toBe("postgresql:///postgres?host=/run&port=5444")
  })

  it("is none where neither the environment nor a file states one", async () => {
    expect(await stated({}, path.join(root, "absent"))).toBeUndefined()
    expect(await stated({}, envFile("other-keys", "OTHER=x\nSTORE_TEST_URL=\n"))).toBeUndefined()
  })
})
