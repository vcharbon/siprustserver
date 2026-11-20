/**
 * The reference pack's suites. Their timeouts are generous: a loaded box
 * stretches every step into a slow pass, never a spurious timeout.
 */
import { defineConfig } from "vitest/config"

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    testTimeout: 300_000,
    hookTimeout: 300_000
  }
})
