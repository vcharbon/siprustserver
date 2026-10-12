/**
 * The real `pivot-schema` the conformance suites hold the TypeScript mirrors
 * to: `PIVOT_SCHEMA_BIN`, else the release build the toolchain defaults to
 * (`Binaries.releaseBinary`).
 *
 * A binary-bound block runs under {@link describeBinary}: where the binary is
 * not on disk each of its tests FAILS, naming the path tried and how to build
 * it. `TOOLCHAIN_TEST_SKIP=1` is the one way to skip them instead.
 */
import { Binaries } from "@sip/toolchain"
import * as fs from "node:fs"
import { beforeEach, describe } from "vitest"

/** The binary the binary-bound blocks run. */
export const BIN: string = process.env["PIVOT_SCHEMA_BIN"] ?? Binaries.releaseBinary("pivot-schema")

/** Whether the run opted out of the binary-bound blocks. */
const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"

/** A `describe` held to the binary: skipped on the opt-out, failed where the binary is missing. */
export const describeBinary = (name: string, body: () => void): void => {
  describe.skipIf(skipping)(name, () => {
    beforeEach(() => {
      if (BIN.length === 0 || !fs.existsSync(BIN)) {
        throw new Error(
          `no pivot-schema at ${BIN}: build it with \`cargo build --release -p pivot-schema\`, or name one ` +
            "with PIVOT_SCHEMA_BIN, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1"
        )
      }
    })
    body()
  })
}
