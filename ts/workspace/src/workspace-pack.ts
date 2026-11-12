/**
 * What a tooling states of its workspace: the variables it reads its
 * deployment from, its configuration file's name, the directories it adds to
 * the generic ones, the store's database name and port in the workspace's
 * own postgres, and the command that makes a workspace. Every generic module of
 * this package reads these through it and names none itself.
 */

/** The environment variables a deployment is read from, by role. */
export interface Variables {
  /** Names the workspace directory. */
  readonly workspace: string
  /** The artifact store. */
  readonly artifacts: string
  /** The session tree. */
  readonly sessions: string
  /** The census runs. */
  readonly census: string
  /** The one pcap root. */
  readonly pcaps: string
  /** Pcap roots beside the one, which a workspace states empty. */
  readonly pcapRoots: string
  /** The lock root. */
  readonly locks: string
  /** The prebuilt binary directory. */
  readonly binDir: string
  /** The address the site binds. */
  readonly bind: string
  /** The origin every printed link is built under. */
  readonly publicUrl: string
  /** The site's port. */
  readonly port: string
}

/** The variable naming the store's database, whatever the tooling. */
export const DATABASE_URL = "DATABASE_URL"

/** A tooling's workspace facts; `D` names the directories it adds. */
export interface WorkspacePack<D extends string = string> {
  readonly variables: Variables
  /** The configuration file at the workspace's root. */
  readonly configFile: string
  /** The tooling's own fixed subdirectories, by role, beside the generic ones. */
  readonly directories: Readonly<Record<D, string>>
  /** The store's database name in the workspace's own postgres. */
  readonly database: string
  /** The port the workspace's own postgres listens on where `database.url` states none. */
  readonly databasePort: number
  /** The command line that makes a workspace, its directory appended. */
  readonly initCommand: string
  /** The site's port where its variable states none. */
  readonly sitePort: number
}
