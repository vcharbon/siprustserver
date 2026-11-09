/** A copied link is built under the site's public URL where the server states one, under the page's own origin otherwise. */
import { describe, expect, it } from "vitest"
import type { ServerConfig } from "../src/api.ts"
import { linkOrigin } from "../src/server-config.ts"

const config = (publicUrl: string | undefined): ServerConfig => ({
  configPath: "c.json",
  caseRoots: [],
  runRoots: [],
  flowsRoots: [],
  port: 1,
  ...(publicUrl === undefined ? {} : { publicUrl })
})

describe("the origin of a copied link", () => {
  it("is the site's public URL, without a trailing slash", () => {
    expect(linkOrigin(config("http://site.example.test:5180"), "http://127.0.0.1:5180")).toBe("http://site.example.test:5180")
    expect(linkOrigin(config("https://site.example.test/"), "http://127.0.0.1:5180")).toBe("https://site.example.test")
  })

  it("is the page's own origin where the server states none or does not answer", () => {
    expect(linkOrigin(config(undefined), "http://127.0.0.1:5180")).toBe("http://127.0.0.1:5180")
    expect(linkOrigin(config(""), "http://127.0.0.1:5180")).toBe("http://127.0.0.1:5180")
    expect(linkOrigin(undefined, "http://127.0.0.1:5180")).toBe("http://127.0.0.1:5180")
  })
})
