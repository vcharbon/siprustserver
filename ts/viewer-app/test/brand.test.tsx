/**
 * The top bar shows the site's name as the server's config states it, linking
 * to the index, and the bundle's build stamp: commit and build time.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { ServerConfig } from "../src/api.ts"
import { Brand, Stamp } from "../src/components/Brand.tsx"
import { siteNameOf } from "../src/server-config.ts"

const config = (name: string | undefined): ServerConfig => ({
  configPath: "c.json",
  caseRoots: [],
  runRoots: [],
  flowsRoots: [],
  port: 1,
  ...(name === undefined ? {} : { name })
})

/** The markup's text, tags dropped. */
const text = (markup: string): string => markup.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim()

describe("the top bar's identity", () => {
  it("shows the site's name, linking to the index", () => {
    const markup = renderToStaticMarkup(<Brand name="Rig One · Pack One" />)
    expect(text(markup)).toBe("Rig One · Pack One")
    expect(markup).toContain(`href="/"`)
  })

  it("reads the site's name from the server's config, none where it states none", () => {
    expect(siteNameOf(config("Rig One · Pack One"))).toBe("Rig One · Pack One")
    expect(siteNameOf(undefined)).toBeUndefined()
    expect(siteNameOf(config(undefined))).toBeUndefined()
    expect(siteNameOf(config(" "))).toBeUndefined()
  })

  it("shows the build's commit and its UTC minute", () => {
    const markup = renderToStaticMarkup(<Stamp stamp={{ commit: "0a1b2c3", builtAt: "2026-11-12T18:04:59.123Z" }} />)
    expect(text(markup)).toBe("0a1b2c3 · 2026-11-12 18:04 UTC")
  })

  it("says where the build read no commit, and where no build stamped the bundle", () => {
    const noCommit = renderToStaticMarkup(<Stamp stamp={{ commit: null, builtAt: "2026-11-12T18:04:00.000Z" }} />)
    expect(text(noCommit)).toBe("no commit · 2026-11-12 18:04 UTC")
    expect(text(renderToStaticMarkup(<Stamp stamp={undefined} />))).toBe("unstamped")
  })
})
