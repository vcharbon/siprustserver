/**
 * The SDK module a workspace's files import, as the tooling states it: the
 * specifier they import it by, the module standing for it when they run, and
 * the declaration text standing for it when they are typechecked. A
 * workspace holds no module of the tooling: both are the tooling's own.
 */

/** The SDK as a run imports it: `specifier` resolves to the tooling's module at `entry`. */
export interface SdkModule {
  /** The specifier a workspace file imports the SDK by. */
  readonly specifier: string
  /** The URL of the tooling's module the specifier resolves to. */
  readonly entry: string
}

/** The SDK as a typecheck reads it: `specifier` maps to the declaration text `text`. */
export interface SdkText {
  /** The specifier a workspace file imports the SDK by. */
  readonly specifier: string
  /** The SDK's `.d.ts` text. */
  readonly text: string
}

/** Where the declaration text of the SDK `specifier` stands for the compiler: a path no file of a workspace takes. */
export const declarationPathOf = (specifier: string): string => `/${specifier.replace("/", "-")}-sdk/index.d.ts`
