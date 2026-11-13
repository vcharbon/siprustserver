/**
 * The base's workspace (`@sip/workspace`) for a pack that ships no workspace
 * command of its own: the `SIPRIG_*` variables, `rig.toml` with the generic
 * sections alone, no directory beyond the generic ones, the store's `siprig`
 * database in the workspace's own postgres on port 5499, the site on 5175,
 * the rig path over the pack `SIPRIG_PACK` names and the workspace, and the
 * campaign tags its `rig.toml` declares.
 */
import { RigPath } from "@sip/rig"
import { ConfigFile, Layout, WorkspaceEnv, type WorkspacePack } from "@sip/workspace"
import * as CampaignTags from "@sip/workspace/campaign-tags"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"

export const PACK: WorkspacePack.WorkspacePack<never> = {
  variables: {
    workspace: "SIPRIG_WORKSPACE",
    artifacts: "SIPRIG_ARTIFACTS",
    sessions: "SIPRIG_SESSIONS",
    census: "SIPRIG_CENSUS",
    pcaps: "SIPRIG_PCAPS",
    pcapRoots: "SIPRIG_PCAP_ROOTS",
    locks: "SIPRIG_LOCKS",
    binDir: "SIPRIG_BIN_DIR",
    bind: "SIPRIG_SITE_BIND",
    publicUrl: "SIPRIG_SITE_URL",
    port: "SIPRIG_SITE_PORT"
  },
  configFile: "rig.toml",
  directories: {},
  database: "siprig",
  databasePort: 5499,
  initCommand: "siprig-workspace init",
  sitePort: 5175
}

/** `rig.toml` read with the generic sections alone. */
export const RigToml = ConfigFile.make(PACK, { fields: {}, campaignFields: {}, check: () => undefined })

export const { configOf, apply } = WorkspaceEnv.make(PACK, RigToml.decodeText)

export const { layoutOf, of } = Layout.make(PACK)

/** No tag declared: a run with no workspace, or one holding no `rig.toml`. */
const NO_TAGS: CampaignTags.Declared = { file: "", tags: {}, tagCases: [] }

/**
 * The campaign tags the workspace at `root` declares in its `rig.toml`
 * (`[tags]`, `[[tag_cases]]`): none where `root` is undefined or holds no
 * `rig.toml`; a `rig.toml` that does not read is refused, naming it.
 */
export const declaredOf = (root: string | undefined) =>
  Effect.gen(function*() {
    if (root === undefined) return NO_TAGS
    const layout = layoutOf(root)
    const held = yield* (yield* FileSystem.FileSystem).exists(layout.config).pipe(Effect.orElseSucceed(() => false))
    if (!held) return NO_TAGS
    return yield* CampaignTags.declaredIn(Effect.map(configOf(layout), (config) => ({ file: layout.config, config }))).pipe(
      Effect.mapError((refused) => refused.reason)
    )
  })

/** The variable naming the pack's directory in the image. */
export const PACK_VARIABLE = "SIPRIG_PACK"

/** The pack's directory `env` names, `undefined` where it names none. */
export const packDirOf = (env: NodeJS.ProcessEnv): string | undefined =>
  env[PACK_VARIABLE] === "" ? undefined : env[PACK_VARIABLE]

/**
 * The rig path over the pack `env` names and the workspace at `layout`
 * loaded (`@sip/rig` `RigPath.load`): a plug-in the rig refuses refuses the
 * workspace at `check`, naming its file, as the site would at start.
 */
export const verifyRigPath = (env: NodeJS.ProcessEnv) => (layout: Layout.Base) =>
  Effect.asVoid(RigPath.load({ pack: packDirOf(env), workspace: layout.root }))

/** The `rig.toml` a new workspace starts with: every key commented out at its default. */
export const TEMPLATE = `# The workspace's configuration. Every key is optional; an exported variable
# of the same meaning wins over the value stated here.

[site]
# The site's port (SIPRIG_SITE_PORT). Default: ${PACK.sitePort}.
# port = ${PACK.sitePort}
# The address the site binds (SIPRIG_SITE_BIND). Default: 127.0.0.1.
# bind = "0.0.0.0"
# The origin every printed link is built under (SIPRIG_SITE_URL). Default: the
# bound address and port, the machine's \`hostname -f\` for a wildcard bind.
# public_url = "http://rig.example.org:${PACK.sitePort}"

[database]
# The result store (DATABASE_URL). Default: ${Layout.databaseUrlOf(PACK)},
# the workspace's own postgres.
# url = "postgresql://siprig@127.0.0.1:${PACK.databasePort}/${PACK.database}"
`

export * as BaseWorkspace from "./base-workspace.ts"
