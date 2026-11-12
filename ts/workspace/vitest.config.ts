import { defineConfig } from "vitest/config"

/** The suites under `test/`; a loaded box stretches every step, so its bounds are wide. */
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    testTimeout: 300_000,
    hookTimeout: 300_000
  }
})
