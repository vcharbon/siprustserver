/**
 * The workspace's one configuration file: the site's port, bind address and
 * public URL, the store's database URL, the prebuilt binary directory, the
 * campaign tags its definer declares (`[tags.<name>]`, `[[tag_cases]]`), the
 * named campaigns it runs (`[campaign.<name>]`), and its collection levels
 * (`[collection]`, `@sip/rig` `Collection`: their names and default read here,
 * what they name checked against the rig path before a job opens); beside
 * them, the sections and campaign keys the tooling adds ({@link Sections}).
 *
 * Every key is optional; a key the schema does not name, or a value of the
 * wrong shape, is refused naming its key, because a typo in a configuration
 * file is otherwise silence.
 */
import { Collection } from "@sip/rig"
import { issueLine, tomlOf } from "@sip/rig/manifest-file"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as PublicUrl from "./public-url.js"
import type { WorkspacePack } from "./workspace-pack.js"

const Port = Schema.Int.check(Schema.isBetween({ minimum: 1, maximum: 65535 }))

const Text = Schema.String.check(Schema.isNonEmpty())

const Site = Schema.Struct({
  /** The site's port variable. */
  port: Schema.optionalKey(Port),
  /** The site's bind variable. */
  bind: Schema.optionalKey(Text),
  /** The site's public-URL variable. */
  public_url: Schema.optionalKey(Text)
})

const Database = Schema.Struct({
  /** `DATABASE_URL`: a `postgresql://` URL, host, port and user included where the server needs them. */
  url: Schema.optionalKey(Text)
})

const Tools = Schema.Struct({
  /** The prebuilt binary directory variable, relative to the workspace root or absolute. */
  bin_dir: Schema.optionalKey(Text)
})

/** A campaign tag: what its cases do, and what a lane does with them unless its file says otherwise. */
export const Tag = Schema.Struct({
  /** What the campaign page offers it as. */
  label: Text,
  /** What the page shows before a run takes its cases. */
  warning: Schema.optionalKey(Text),
  /** `exclude`: a campaign leaves its cases out on a lane stating nothing of it; `run`: it runs them. */
  default: Schema.Literals(["exclude", "run"]),
  /** Whether its cases disrupt tests running at the same time: each runs with nothing else on its lane. */
  alone: Schema.optionalKey(Schema.Boolean)
})
export interface Tag extends Schema.Schema.Type<typeof Tag> {}

/** A tag put on case files that cannot state it themselves, such as built-in ones. */
export const TagCases = Schema.Struct({
  tag: Text,
  /** Globs over case file names, as `--cases` reads them. */
  cases: Schema.Array(Text)
})
export interface TagCases extends Schema.Schema.Type<typeof TagCases> {}

/** The keys of a named campaign every tooling reads: its own page and run history, over the case files its globs match. */
export const CAMPAIGN_FIELDS = {
  /** Its name on the page. */
  title: Schema.optionalKey(Text),
  /** Globs over case file names, as `--cases` reads them. */
  cases: Schema.NonEmptyArray(Text),
  /** The lanes a run takes when it names none; absent, the first runnable lane's group. */
  lanes: Schema.optionalKey(Schema.NonEmptyArray(Text)),
  /** Tags the campaign opts into by itself, as `--allow` does. */
  allow: Schema.optionalKey(Schema.Array(Text)),
  /**
   * The operations of the rig path a run injects after provisioning, in order,
   * and heals in reverse at its end, however it ends (`@sip/rig` `Setup`).
   */
  setup: Schema.optionalKey(Schema.Array(Text))
}

const NamedCampaign = Schema.Struct(CAMPAIGN_FIELDS)

/** A named campaign as every tooling reads it. */
export interface NamedCampaign extends Schema.Schema.Type<typeof NamedCampaign> {}

/** The sections every tooling's file holds, the named campaigns and the collection levels apart. */
const BASE_FIELDS = {
  site: Schema.optionalKey(Site),
  database: Schema.optionalKey(Database),
  tools: Schema.optionalKey(Tools),
  tags: Schema.optionalKey(Schema.Record(Schema.String, Tag)),
  tag_cases: Schema.optionalKey(Schema.Array(TagCases))
}

/** The configuration as every tooling reads it. */
export interface Base extends Schema.Schema.Type<Schema.Struct<typeof BASE_FIELDS>> {
  readonly campaign?: Readonly<Record<string, NamedCampaign>>
  readonly collection?: Collection.Collection
}

/** Struct fields that decode with no service, as every key of the file does. */
export type Fields = { readonly [x: string]: Schema.Struct.Fields[string] & { readonly "DecodingServices": never } }

/** What a tooling adds to the file: its own sections, its own keys of a named campaign, and the checks its values' shapes cannot state. */
export interface Sections<F extends Fields, C extends Fields> {
  readonly fields: F
  readonly campaignFields: C
  /** Why a configuration that reads does not hold for the tooling, naming the key; `undefined` where it holds. */
  readonly check: (config: Config<F, C>) => string | undefined
}

const schemaOf = <F extends Fields, C extends Fields>(fields: F, campaignFields: C) =>
  Schema.Struct({
    ...BASE_FIELDS,
    ...fields,
    campaign: Schema.optionalKey(Schema.Record(Schema.String, Schema.Struct({ ...CAMPAIGN_FIELDS, ...campaignFields }))),
    /** The collection levels a job picks among, and the default one: its shape is `@sip/rig`'s, read by `parse`. */
    collection: Schema.optionalKey(Schema.Record(Schema.String, Schema.Unknown))
  })

/** The configuration of a tooling adding the sections `F` and the campaign keys `C`. */
export type Config<F extends Fields, C extends Fields> =
  & Omit<Schema.Schema.Type<ReturnType<typeof schemaOf<F, C>>>, "collection">
  & { readonly collection?: Collection.Collection }

/** Why a file is no configuration: the file, and the key at fault where one is. */
export class Invalid extends Schema.TaggedError<Invalid>()("ConfigFile.Invalid", {
  file: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.file}: ${this.reason}`
  }
}

/** A tag's name stands in a comma-separated `--allow` and in a skip's reason; a campaign's names session directories and URLs. */
const SLUG = /^[a-z0-9][a-z0-9-]*$/

/** Why a named campaign does not hold: a name that is no slug, or a tag it allows that no one declared. */
const campaignMeaningOf = (config: Base): string | undefined => {
  for (const [name, campaign] of Object.entries(config.campaign ?? {})) {
    if (!SLUG.test(name)) return `campaign.${name}: a campaign name is lower-case letters, digits and dashes`
    const unknown = (campaign.allow ?? []).filter((tag) => !Object.hasOwn(config.tags ?? {}, tag))
    if (unknown.length > 0) return `campaign.${name}.allow: names ${unknown.join(", ")}, no declared tag`
  }
  return undefined
}

/** The checks a value's shape cannot state: a tag's name, a named campaign, a public URL `public-url.ts` takes, a postgres URL. */
const meaningOf = (pack: WorkspacePack, config: Base): string | undefined => {
  const misnamed = Object.keys(config.tags ?? {}).find((name) => !SLUG.test(name))
  if (misnamed !== undefined) return `tags.${misnamed}: a tag name is lower-case letters, digits and dashes`
  const campaign = campaignMeaningOf(config)
  if (campaign !== undefined) return campaign
  const stated = config.site?.public_url
  if (stated !== undefined) {
    const setting = { stated, bind: PublicUrl.DEFAULT_BIND, port: pack.sitePort }
    const derived = PublicUrl.derive(pack.variables, setting, "")
    if (Result.isFailure(derived)) return `site.public_url: ${derived.failure.slice(pack.variables.publicUrl.length + 1)}`
  }
  const url = config.database?.url
  if (url !== undefined) {
    const parsed = URL.parse(url)
    if (parsed === null || (parsed.protocol !== "postgresql:" && parsed.protocol !== "postgres:")) {
      return `database.url: not a postgresql:// URL: ${url}`
    }
  }
  return undefined
}

/** The reading of `pack`'s configuration file, its sections added: its schema, {@link parse} and {@link decodeText}. */
export const make = <F extends Fields, C extends Fields>(
  pack: WorkspacePack,
  sections: Sections<F, C>
) => {
  const schema = schemaOf(sections.fields, sections.campaignFields)
  const decoder = schema as unknown as Schema.Decoder<Readonly<Record<string, unknown>>>
  const decode = Schema.decodeUnknownResult(decoder, { onExcessProperty: "error", errors: "first" })
  const parse = (file: string, text: string): Result.Result<Config<F, C>, Invalid> => {
    const invalid = (reason: string) => Result.fail(new Invalid({ file, reason }))
    const raw = tomlOf(text)
    if (Result.isFailure(raw)) return invalid(raw.failure)
    const decoded = decode(raw.success)
    if (Result.isFailure(decoded)) return invalid(issueLine(decoded.failure.message, pack.configFile))
    const { collection, ...rest } = decoded.success
    const levels = collection === undefined ? Result.succeed(undefined) : Collection.collectionOf(collection)
    if (Result.isFailure(levels)) return invalid(`collection.${levels.failure}`)
    const named = Collection.levelsOf(levels.success)
    if (Result.isFailure(named)) return invalid(named.failure)
    const config = (levels.success === undefined ? rest : { ...rest, collection: levels.success }) as Config<F, C>
    const unmeant = meaningOf(pack, config) ?? sections.check(config)
    return unmeant === undefined ? Result.succeed(config) : invalid(unmeant)
  }
  const decodeText = (file: string, text: string): Effect.Effect<Config<F, C>, Invalid> =>
    Result.match(parse(file, text), { onSuccess: Effect.succeed, onFailure: Effect.fail })
  return { Config: schema, parse, decodeText }
}
