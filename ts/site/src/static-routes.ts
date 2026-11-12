/** A built app's files, from `staticDir`; its page URLs are `ShellRoutes`'. */
import { Effect, Layer } from "effect"
import { HttpStaticServer } from "effect/unstable/http"
import { ViewerConfig } from "./config.ts"
import { LazyFilePlatform } from "./lazy-file-platform.ts"

export const StaticRoutes = Layer.unwrap(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    return config.staticDir === undefined
      ? Layer.empty
      : HttpStaticServer.layer({ root: config.staticDir, spa: true }).pipe(Layer.provide(LazyFilePlatform))
  })
)
