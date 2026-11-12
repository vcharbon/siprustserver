/**
 * A collector written with `defineCollector`, for its test: `open` leaves the
 * step it ran as in its state; `collect` attaches, per cell, a log naming what
 * `open` left and the cell, and answers `module-check` for it, or fails when
 * its `fail` parameter is true, and logs and prints to the console before it
 * answers when its `chatty` parameter is true; `close` appends `closed` to
 * the file its `trace` parameter names.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import { defineCollector } from "../../../src/define-collector.js"

defineCollector({
  open: (io) => Effect.sync(() => fs.writeFileSync(path.join(io.state, "opened"), io.step)),
  collect: (io) =>
    Effect.gen(function*() {
      if (io.context.params.fail === true) return yield* Effect.fail(new Error("the log source is gone"))
      if (io.context.params.chatty === true) {
        yield* Effect.log("collecting, said through the logger")
        yield* Effect.logWarning("a warning, said through the logger")
        yield* Console.log("collecting, said through the console")
      }
      const opened = fs.readFileSync(path.join(io.state, "opened"), "utf8")
      for (const cell of io.context.cells) {
        yield* io.attach({ cell: cell.id, kind: "log", title: "its log", file: `${cell.id}.log`, content: `${opened}: ${cell.id}\n` })
        yield* io.answer({ verification: "module-check", cell: cell.id, answer: "pass" })
      }
    }),
  close: (io) => Effect.sync(() => fs.appendFileSync(String(io.context.params.trace), "closed\n"))
})
