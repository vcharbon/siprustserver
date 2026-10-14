/**
 * The CLI FACTS: what a deployment states that the command line needs before
 * any service exists — a flag's default is part of the parser, and the env
 * files have to be applied before the layers that read them are built. Every
 * command builder takes them; everything a command needs once it runs comes
 * from the deployment's services instead (`@sip/corpus` `deployment/`).
 */

/**
 * Where a host keeps the environment a command needs and an operator's shell
 * may not carry. A file is a DEFAULT, never an override: a key already
 * exported is left as the caller set it, and only the keys listed here are
 * applied, so a file cannot set an environment nobody documented.
 */
export interface HostEnv {
  /** The files read, in order; the first to state a key wins. */
  readonly files: ReadonlyArray<string>
  /** The keys a file may set, each with what it is for. */
  readonly keys: Readonly<Record<string, string>>
  /** The keys whose value is a place on disk, resolved against the directory of the file that states it. */
  readonly pathKeys: ReadonlySet<string>
}

export interface CliFacts {
  /** The lane a command drives where its line names none. */
  readonly defaultLane: string
  readonly hostEnv: HostEnv
  /** The lines every entry point's `--help` ends with, so any one of them leads to the others. */
  readonly frontDoors: string
  /**
   * Why this checkout may not run `what`, a command that writes what the
   * deployment's current population owns (the corpus, a lane's fixed ports);
   * `null` where it may. Read when the command runs.
   */
  readonly environmentRefusal: (what: string) => string | null
}
