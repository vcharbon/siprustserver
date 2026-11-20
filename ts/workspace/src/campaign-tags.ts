/**
 * The campaign tags a deployment declares, read anew at each call so the CLI
 * and the site read one file the same way (`deployment-config.ts`).
 */
import type { Refused } from "@sip/cli/host-env"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import type * as ConfigFile from "./config-file.js"
import type * as DeploymentConfig from "./deployment-config.js"

/** Every declared tag by name, and the tags put on case files by glob. */
export interface Declared {
  /** The file stating them. */
  readonly file: string
  readonly tags: Readonly<Record<string, ConfigFile.Tag>>
  readonly tagCases: ReadonlyArray<ConfigFile.TagCases>
}

/** What the deployment `stated` reads declares. */
export const declaredIn = (
  stated: Effect.Effect<DeploymentConfig.Stated<ConfigFile.Base>, Refused, FileSystem.FileSystem>
): Effect.Effect<Declared, Refused, FileSystem.FileSystem> =>
  Effect.map(stated, ({ config, file }) => ({
    file,
    tags: config.tags ?? {},
    tagCases: config.tag_cases ?? []
  }))
