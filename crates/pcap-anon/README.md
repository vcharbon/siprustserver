# pcap-anon

`pcap-anon` makes a SIP capture shareable. It reads the flows document that
`sipflow --json` (crate `sip-pcap`) writes from a capture, replaces every
identity in it with a fake, checks that nothing of the raw document is left,
and writes the anonymised document. `sipflow --to-pcap` turns that document
back into a capture file. It is offline tooling. No runner links it.

The anonymiser knows nothing of a network until you tell it. You give it three
files: a numbering **plan**, a network **profile** and a **reading**. You also
give it a **seed** file, which the tool keeps for you.

## Run it

```sh
sipflow --json --reading my.reading capture.pcapng > capture.flows.json

# One anonymised document, checked, under a new seed kept at capture.seed:
pcap-anon anonymize capture.flows.json capture.anon.flows.json \
  --plan my.plan.json --reading my.reading --profile my.profile.json \
  --seed-file capture.seed --fresh-seed --verify

# A corpus entry: the anonymised document, the callflow text and a summary:
pcap-anon auto capture.flows.json out/ --capture capture \
  --plan my.plan.json --profile my.profile.json --reading my.reading \
  --seed-file capture.seed --verify

sipflow --to-pcap capture.anon.flows.json --out capture.anon.pcap
```

- `anonymize` writes one flows document. `--check-reproducible [--sample N]`
  writes nothing. It anonymises the raw document twice under one seed. It
  passes when the two outputs are byte-identical and the leak checks count
  nothing. For each token that no fake explains, it reports the header and
  the shape (`9` for a digit, `a`/`A` for a letter), never the value.
- `auto` writes `<capture>.anon.flows.json`, `source.txt` and `auto.json`.
  `source.txt` is the callflow as text, for a reader who has no capture
  viewer. Its first line is the format line `pcap2test source v1`.
  `auto.json` gives the counts of the run, or the error. It never contains
  the seed. With `--as-is`, `auto` takes the document as it is and
  anonymises nothing.
- Both commands refuse a document that is already anonymised. The output
  contains `"rewritten": "anonymised"`, and that mark is the only way to
  tell a fake from a real value. To make an output again, start from the raw
  capture under its kept seed.

## What it rewrites

- **Numbers.** Every `sip:`/`sips:`/`tel:` user part of four digits or more,
  in every header, in the bodies it reads, in a PASSporT's claims
  (RFC 8224) and in a hex User-to-User payload (RFC 7433). The plan finds
  one number under all its forms (`+CC NSN`, `00CC NSN`, national `0 NSN`,
  a trunk dial form, a service prefix). Each form becomes the same fake, of
  the same width, in a reserved digit space. A run that the plan does not
  recognise is mapped whole. It keeps its CC-looking head, and the report
  counts it as `unrecognised`, so you can improve the plan from a count and
  not from a leak. Any other digit run of six digits or more that is not a
  protocol counter is mapped the same way.
- **Names.** Hosts and domains, Via `received`/`maddr`, realms, charging
  ids, `Server`/`User-Agent`/`Organization`/`Subject` texts, the free
  fields of an SDP part, and user parts that are names. Each is faked word
  by word, keeps its case, and gets the same fake wherever it appears.
  Display names keep their punctuation and length: every letter becomes
  `x`/`X`.
- **Addresses.** IPv4 and IPv6, wherever they are written. A fake keeps the
  width of the text and comes from a range that no peer uses: `0.0.0.0/8`
  and `240.0.0.0/4`, or `fd00::/8`.
- **Dialog and transaction identifiers.** Call-ID, tags, branches,
  `Replaces` and the related ones. Each word is mapped one to one, so no
  two dialogs merge.
- **Bodies.** It reads SDP, sipfrag, media-server control (MSCML), DTMF
  relay and eCall control, plus the types that the profile states. It
  redacts any other part whole to `x` and keeps its framing and line
  lengths.

## What it keeps

The protocol counters (CSeq, RSeq/RAck, Content-Length, Expires and the
other counter fields, the SDP `o=` session id and version, ports), the
protocol text (a reason phrase, a Reason or Warning text, media-server
control values), the protocol vocabulary, a one-character name, the special-use host names
(`invalid`, `test`, `example`, `localhost`) and the documentation addresses.
It also keeps the addresses and texts that the profile keeps (see below).
The seed is never written in the output.

## The plan

```json
{
  "default_cc": "28",
  "countries": [
    { "cc": "1", "nsn_len": 10 },
    { "cc": "28", "nsn_len": 8, "national_prefix": "0", "short_len": 5 }
  ],
  "trunks": [{ "prefix": "15550", "len": 7 }],
  "private_len": { "min": 4, "max": 7 },
  "max_composed_prefix": 6,
  "fake_prefix": "000"
}
```

- `countries`: the length of the national significant number for each
  country code. `national_prefix` enables the national form, for
  `default_cc` only. `short_len` accepts `+CC` followed by exactly that many
  digits as a short code.
- `trunks`: the trunk ids that can come before `CCNSN` in a dial form.
- `private_len`: the lengths of the all-digit numbers that are private
  numbers.
- `max_composed_prefix`: the most digits of a service prefix between the CC
  and the NSN.
- `fake_prefix`: the start of the reserved digit space. A fake is
  `fake_prefix`, the marker `9`, then digits of `HMAC-SHA256(seed, number)`.

The example uses country code 28, which the ITU has not assigned. The
crate's tests use the same plan (`tests/data/test.plan.json`).

## The profile

```json
{
  "sut": ["10.0.0.0/24", "192.0.2.7"],
  "kept_texts": ["Exampleswitch 4.2"],
  "bodies": [{ "media_type": "application/vnd.example.order", "read": "digit-runs",
               "vocabulary": ["order"] }]
}
```

- `sut`: the addresses (IPs or CIDR blocks) of the system under test. By
  default the tool keeps them, so that a later cut can still find the
  system. With `--all-addresses` it maps them too.
- `kept_texts`: the strings that the system itself writes, such as its
  product name. The tool keeps them as written, and `--verify` accepts them.
  `--all-addresses` maps them.
- `bodies`: the media types that the network adds, and how to read each:
  - `digit-runs`: every digit run is a dial form and is mapped. The other
    words stay, and `--verify` reports each word that the row's `vocabulary`
    does not list.
  - `control`: an XML control document. Its attribute values and texts are
    rewritten where they are.

Which addresses get mapped depends on the scope:

- The default scope maps the public addresses and keeps the private ranges
  and the `sut` addresses.
- `--all-addresses` maps every address a peer can hold. Use it for a
  document that leaves the network.
- `--keep-addresses` maps no address. Use it for a document that stays on
  the network's own tools.

## The reading

The reading is the file that `sipflow --reading` read the capture under:
`key = value` lines that state the `exemptions` and the bounds
`max-header-length`, `max-uri-length`, `max-header-lines` and `max-datagram`.
The anonymiser parses every message the way the extraction did. The tool
has no default plan, profile or reading, and refuses to run without each one.

## The seed

The seed is 32 random bytes. It keys every fake and inverts the mapping, so
treat it like a password.

- It is read only from `--seed-file`, never from the command line.
- It is written with mode 600, and a seed file that anyone else can read is
  refused.
- It is never printed.
- `--fresh-seed` draws a new seed and keeps it. The tool refuses a fresh
  seed where a seed is already kept, because a new seed gives a different
  capture.
- Under its kept seed, a capture comes out byte-identical every time.

## What `--verify` checks

The tool writes nothing (with `auto`, only `auto.json`) when one of these
counts is not zero:

- **numbers survive**: a number-bearing user part of the raw document is
  still found as a whole digit run where the raw bytes held it.
- **display names stay unmasked**: a display name still has a letter other
  than `x`/`X`, or a non-ASCII byte.
- **names survive**: a name that the pass maps is still found where the raw
  bytes held it.
- **dialog identifiers merge**: the output has fewer distinct Call-IDs,
  branches or tags than the raw document.
- **domains, addresses, words, non-ASCII words and digit runs no fake
  explains**: the output is checked alone, against the set of fakes that
  the run wrote. A token that is not a fake, not protocol vocabulary, not a
  special-use name and not a kept address or text is counted.

## Residuals

These are documented and not handled:

- An identity that a peer typed into protocol text, for example a name in a
  reason phrase.
- A value that is hex-encoded outside a User-to-User payload, and the
  letters of a text User-to-User payload.
- The length of each line of a redacted part.
- The path of a URL.
- The free words of a vendor header value outside a quoted string.
- The words of an identifier that are shorter than four characters.
- A datagram that the parser refuses: only its addresses and digit runs are
  rewritten. The leak check still reads all of it.

Two different numbers can get the same fake, and a short fake can appear
inside a long one. The run counts these cases and reports them. It does not
fail on them.

## Tests

`cargo nextest run -p pcap-anon`. The unit tests and `tests/it` run on the
invented plan, profile and reading in `tests/data/`. The tests in
`tests/it/samples.rs` extract each capture of `ts/reference-pack/capture/`,
anonymise it with `--verify`, anonymise it again under the kept seed and
compare the two outputs byte for byte. Their profile
(`tests/data/samples.profile.json`) states the B2BUA address of each capture
as the SUT.
