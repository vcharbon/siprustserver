/**
 * The top bar names the rig, then the pack the server runs where its config
 * states one, and shows the bundle's build stamp: commit and build time.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { ServerConfig } from "../src/api.ts"
import { Brand, Stamp, tabTitle } from "../src/components/Brand.tsx"
import { packTitleOf } from "../src/server-config.ts"

const config = (title: string | undefined): ServerConfig => ({
  configPath: "c.json",
  caseRoots: [],
  runRoots: [],
  flowsRoots: [],
  port: 1,
  ...(title === undefined ? {} : { title })
})

/** The markup's text, tags dropped. */
const text = (markup: string): string => markup.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim()

describe("the top bar's identity", () => {
  it("names the rig, linking to the index, and the pack beside it", () => {
    const markup = renderToStaticMarkup(<Brand title="Pack One" />)
    expect(text(markup)).toBe("siprig Pack One")
    expect(markup).toContain(`href="/"`)
    expect(tabTitle("Pack One")).toBe("siprig · Pack One")
  })

  it("names the rig alone until the server states a pack", () => {
    expect(text(renderToStaticMarkup(<Brand title={undefined} />))).toBe("siprig")
    expect(tabTitle(undefined)).toBe("siprig")
    expect(packTitleOf(undefined)).toBeUndefined()
    expect(packTitleOf(config(undefined))).toBeUndefined()
    expect(packTitleOf(config(" "))).toBeUndefined()
    expect(packTitleOf(config("Pack One"))).toBe("Pack One")
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
