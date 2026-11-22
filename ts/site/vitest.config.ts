import { defineConfig } from "vitest/config"

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    environment: "node",
    // A loaded box stretches every step: a slow pass, never a spurious timeout.
    testTimeout: 300_000,
    hookTimeout: 300_000
  }
})
