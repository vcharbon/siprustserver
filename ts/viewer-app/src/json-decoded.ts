/**
 * A JSON document with its packed strings opened: a string that is base64 of
 * gzip (it starts `H4sI`, the gzip magic in base64) reads as the JSON it
 * unpacks to, or as its text where that is no JSON; a string that does not
 * unpack stays as it is. Objects and arrays are opened member by member.
 */

/** The base64 of gzip's magic bytes and compression method: every gzip text in base64 starts with it. */
const GZIP_BASE64 = /^H4sI[A-Za-z0-9+/]+={0,2}$/

/** `text`, base64 of gzip, unpacked; undefined where it does not unpack. */
const gunzipped = async (text: string): Promise<string | undefined> => {
  try {
    const bytes = Uint8Array.from(atob(text), (c) => c.charCodeAt(0))
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"))
    return await new Response(stream).text()
  } catch {
    return undefined
  }
}

/** `value` with every packed string opened. */
export const jsonDecoded = async (value: unknown): Promise<unknown> => {
  if (typeof value === "string") {
    if (!GZIP_BASE64.test(value)) return value
    const text = await gunzipped(value)
    if (text === undefined) return value
    try {
      return await jsonDecoded(JSON.parse(text))
    } catch {
      return text
    }
  }
  if (Array.isArray(value)) return Promise.all(value.map(jsonDecoded))
  if (typeof value === "object" && value !== null) {
    const entries = await Promise.all(Object.entries(value).map(async ([key, one]) => [key, await jsonDecoded(one)] as const))
    return Object.fromEntries(entries)
  }
  return value
}
