/**
 * A refused forced drop is retried while postgres refuses it as not permitted
 * (SQLSTATE 42501): the refusal as the Effect SQL client reports it.
 */
import { describe, expect, it } from "vitest"
import { AuthorizationError, SqlError, SqlSyntaxError } from "effect/unstable/sql/SqlError"
import * as Client from "../src/client.js"

/** The `pg` driver's error for SQLSTATE `code`. */
const pgError = (code: string, message: string): Error => Object.assign(new Error(message), { code })

describe("Client.notPermitted", () => {
  it("reads postgres' refusal to terminate a backend as not permitted", () => {
    const cause = pgError("42501", "permission denied to terminate process")
    const error = new SqlError({
      reason: new AuthorizationError({ cause, message: "Failed to execute statement", operation: "execute" })
    })
    expect(Client.notPermitted(error)).toBe(true)
  })

  it("reads any other refusal as final", () => {
    const cause = pgError("42601", "syntax error")
    const error = new SqlError({
      reason: new SqlSyntaxError({ cause, message: "Failed to execute statement", operation: "execute" })
    })
    expect(Client.notPermitted(error)).toBe(false)
  })
})
