/**
 * The site's public URL: the public-URL variable where stated, else the
 * address the site binds and its port, a wildcard bind named by the
 * machine's name; every variable is the tooling's, and a refusal names it.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import { describe, expect, it } from "vitest"
import * as Generic from "../src/public-url.js"
import { PACK } from "./pack.js"

const SITE: Generic.SiteVariables = { ...PACK.variables, defaultPort: 5175 }

const PublicUrl = { ...Generic, ...Generic.make(SITE) }

const HOST = "tools.example.com"

const derived = (env: NodeJS.ProcessEnv, port?: number): string | undefined => {
  const setting = PublicUrl.settingOf(env, port)
  if (Result.isFailure(setting)) return undefined
  return Result.getOrUndefined(PublicUrl.derive(setting.success, HOST))
}

describe("the default public URL", () => {
  it("is the loopback site on the tooling's port where nothing is stated", () => {
    expect(derived({})).toBe("http://127.0.0.1:5175")
    expect(Generic.make({ ...SITE, defaultPort: 6100 }).fromEnvSync({})).toBe("http://127.0.0.1:6100")
  })

  it("takes the site's port from the port variable, or the port the caller knows", () => {
    expect(derived({ TOOL_PORT: "5825" })).toBe("http://127.0.0.1:5825")
    expect(derived({ TOOL_PORT: "5825" }, 6001)).toBe("http://127.0.0.1:6001")
  })

  it("names a wildcard bind by the machine's name, and only then reads it", () => {
    expect(derived({ TOOL_BIND: "0.0.0.0" })).toBe(`http://${HOST}:5175`)
    expect(derived({ TOOL_BIND: "::", TOOL_PORT: "5600" })).toBe(`http://${HOST}:5600`)
    const setting = (env: NodeJS.ProcessEnv) => Result.getOrThrow(PublicUrl.settingOf(env))
    expect(PublicUrl.needsHost(setting({ TOOL_BIND: "0.0.0.0" }))).toBe(true)
    expect(PublicUrl.needsHost(setting({}))).toBe(false)
    expect(PublicUrl.needsHost(setting({ TOOL_BIND: "0.0.0.0", TOOL_PUBLIC_URL: "http://x.notreal" }))).toBe(false)
  })

  it("names any other bind by its own address, an IPv6 one bracketed", () => {
    expect(derived({ TOOL_BIND: "192.0.2.10" })).toBe("http://192.0.2.10:5175")
    expect(derived({ TOOL_BIND: "::1" })).toBe("http://[::1]:5175")
    expect(derived({ TOOL_BIND: "  " })).toBe("http://127.0.0.1:5175")
  })

  it("refuses a port variable that is no port, naming it", () => {
    expect(PublicUrl.settingOf({ TOOL_PORT: "abc" })).toEqual(Result.fail("TOOL_PORT is not a port: abc"))
    expect(Result.isFailure(PublicUrl.settingOf({ TOOL_PORT: "70000" }))).toBe(true)
  })

  it("reads none of another tooling's variables", () => {
    expect(derived({ OTHER_BIND: "0.0.0.0", OTHER_PORT: "5825", OTHER_PUBLIC_URL: "http://x.notreal" })).toBe(
      "http://127.0.0.1:5175"
    )
  })
})

describe("a stated public-URL variable", () => {
  it("overrides the bind and the port, without its trailing slash", () => {
    const env = { TOOL_PUBLIC_URL: "https://tools.example.com/", TOOL_BIND: "0.0.0.0", TOOL_PORT: "5825" }
    expect(derived(env)).toBe("https://tools.example.com")
    expect(derived({ TOOL_PUBLIC_URL: "http://192.0.2.10:8080/tools//" })).toBe("http://192.0.2.10:8080/tools")
  })

  it("is unstated where empty", () => {
    expect(derived({ TOOL_PUBLIC_URL: "" })).toBe("http://127.0.0.1:5175")
  })

  it("is refused where it is no http(s) URL, or carries a query or a fragment, naming the variable", async () => {
    for (const stated of ["tools.notreal:5175", "ftp://tools.notreal", "http://tools.notreal/?a=1", "http://tools.notreal/#x"]) {
      const error = await Effect.runPromise(Effect.flip(PublicUrl.fromEnv({ TOOL_PUBLIC_URL: stated })))
      expect(error.message, stated).toMatch(/^TOOL_PUBLIC_URL (is not an http\(s\) URL|carries a query or a fragment): /)
    }
  })
})

describe("the public URL of an environment", () => {
  it("reads the machine's name for a wildcard bind", async () => {
    const url = await Effect.runPromise(PublicUrl.fromEnv({ TOOL_BIND: "0.0.0.0", TOOL_PORT: "5825" }))
    const host = await Effect.runPromise(PublicUrl.hostName)
    expect(host).not.toBe("")
    expect(url).toBe(`http://${host}:5825`)
  })

  it("answers synchronously for a synchronous caller", () => {
    expect(PublicUrl.fromEnvSync({ TOOL_PUBLIC_URL: "http://tools.notreal:5175" })).toBe("http://tools.notreal:5175")
  })
})
