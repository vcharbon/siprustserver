/**
 * What the console reaches when it launches, stops, plans, fingerprints or
 * reads off the disk: the platform, and this deployment's launcher, identity,
 * roots and tools. A layer captures it when it is built and hands it to each
 * call.
 */
import type { Identity, Launcher, Roots, Tools } from "@sip/corpus"
import type * as FileSystem from "effect/FileSystem"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

export type Host =
  | FileSystem.FileSystem
  | ChildProcessSpawner.ChildProcessSpawner
  | Launcher.Service
  | Identity.Service
  | Roots.Service
  | Tools.Service
