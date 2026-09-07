/**
 * The test rig: a stub executable per CLI, and the config that points a service
 * at it.
 *
 * The stubs are shell scripts rather than the real binaries on purpose. What
 * these tests own is the ADAPTER — the argv it builds, the streams it collects,
 * the exit codes it maps — and holding that to a `cargo build` would make it
 * untestable on a fresh checkout. The real binaries are exercised by the
 * env-gated conformance test beside these.
 */
import { NodeServices } from "@effect/platform-node"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Layer from "effect/Layer"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as path from "node:path"

const here = path.dirname(new URL(import.meta.url).pathname)

/** A file this package's tests ship: a stub executable, or an input for one. */
export const fixture = (name: string): string => path.join(here, "fixtures", name)

/**
 * One service layer, wired onto the Node child-process implementation and the
 * env the stubs need — the whole rig a test provides.
 */
export const rig = <A, E>(
  service: Layer.Layer<A, E, ChildProcessSpawner.ChildProcessSpawner>,
  env: Record<string, string>
): Layer.Layer<A, E> =>
  service.pipe(
    Layer.provide(NodeServices.layer),
    Layer.provide(ConfigProvider.layer(ConfigProvider.fromUnknown(env)))
  )
