# `tooling/siprig/` — the image kit

The rig's images (ADR-0060 item 16): the base image `siprig:<tag>`, built from this
repository, and a pack's image `<name>-rig:<tag>`, built from the base one and the pack's
`pack.toml`. The front door is `kit`:

```
kit base [--tag T]                                  siprig:<T>; default the short sha, -dirty for a dirty tree
kit pack <dir> [--tag T] [--base IMAGE]             <name>-rig:<T> from the pack at <dir>, FROM IMAGE
kit tar <dir> [--tag T] [--base IMAGE] [--out DIR]  the pack's image saved as DIR/<name>-rig-<T>.tar.zst, its wrapper beside it
```

`kit pack` defaults `T` to the short sha of the checkout holding the pack and `IMAGE` to
`siprig:<this checkout's short sha>`. It needs this repository's TypeScript installed
(`pnpm -C ts install`): it reads the manifest with `ts/siprig/src/kit-cli.ts`.

## The base image

| Path | Holds |
|---|---|
| `/opt/siprig/ts` | this repository's `ts/` workspace, installed, the viewer app built |
| `/opt/siprig/bin` | `sipflow`, `pivot-schema`, and the plain replay binary as `replay` (`SIPRIG_BIN_DIR`, first on `PATH`) |
| `/opt/siprig/image/bin` | the base's commands, after `/opt/siprig/bin` on `PATH` |
| `/opt/siprig/image/stamp` | one `<component>.env` per checkout built in: `SHA`, `BRANCH`, `DIRTY`, `DIFF`, `DIR` |

Node, postgres (its major in the label `org.siprig.pg-major`), s6-overlay and the docker
CLI; no Python. The s6 services run a workspace mounted at its own path (`SIPRIG_WORKSPACE`):
`workspace` checks and prepares it and writes the store's environment to
`/run/siprig/workspace.env`, `postgres` runs the workspace's own cluster in `pgdata/` on a
private socket, `migrate` brings the store to the image's schema, `site` serves. Each
service runs a base command by name:

| Command | Contract | The base's |
|---|---|---|
| `siprig-workspace init <dir>` | makes what the workspace lacks, keeps the rest | `@sip/workspace` over `rig.toml`'s generic sections |
| `siprig-workspace check [<dir>]` | `<file>: valid`, then `KEY=value` lines: `SIPRIG_SITE_PORT`, `SIPRIG_SITE_BIND`, `DATABASE_URL` among them; exit 2 on a refused file, naming it, so the container stops at start | `rig.toml`, and the rig path over the pack and the workspace |
| `siprig-store migrate` | the store `DATABASE_URL` names (exported, else the workspace's `[database] url`) created and migrated, dumped into the workspace's `backups/` first | the store's own migrations |
| `siprig-site` | the site, on the workspace's port and bind address | `@sip/site` over a site pack with no lane, campaign or rule |

A pack replaces a base command by shipping an executable of the same name among its
binaries: they land in `/opt/siprig/bin`, ahead on `PATH`. A pack's `replay` replaces the
plain one the same way. A pack's `siprig-<verb>` is the wrapper's verb `<verb>`, run in the
running container with the store's environment, marked busy while it runs; a pack's
`siprig-busy` adds its own lines to what `down` and `restart` refuse over. The base's
commands (`in`, `site`, `store`, `workspace`, `busy`) are no verb.

The image's `git` answers the provenance questions of `@sip/toolchain` `git.ts` in a stamp's
`DIR` from that stamp, read as data (a value is never run; a missing key fails, naming it),
and is the real git outside `/opt/siprig`.

## A pack's image

The kit stages the pack's tree as the image receives it, reads its `pack.toml` (`@sip/rig`
`Pack`, ADR-0060 appendix §16) there and refuses, before any build, exit 2, naming the key:
a key the schema does not name; a path the manifest names that the staged tree does not
hold as that key needs it (an ignored file, or one of a nested repository, is not staged),
or that leads out of it once its links are followed; an `[image] binaries` directory the
pack does not hold (it is read where the pack lies: build output its checkout may ignore),
or a link in it to nothing; an `[image] packages` entry that is no Debian package name.
It then builds:

- the pack's tree at `/opt/siprig/pack` (`SIPRIG_PACK`): the files its checkout tracks or
  holds untracked, ignored files left out (the whole directory where it is in no checkout);
- `[image] packages` installed with `apt-get`;
- `[image] binaries`, the directory's files with every link followed, into `/opt/siprig/bin`;
- `[image] setup`, run once in `/opt/siprig/pack` as root, the packages and the tree in
  place, `SIPRIG_BUILD_CACHE` a directory kept between builds;
- the pack's stamp `<component>.env` (`DIR=/opt/siprig/pack`), `SIPRIG_COMPONENT`, and the
  labels `org.siprig.pack`, `org.siprig.wrapper`;
- the wrapper named by `wrapper`, written from `wrapper` here naming the image: at
  `/opt/siprig/image/wrapper` in the image, and beside the tarball `kit tar` writes.

The wrapper (`<wrapper> --help`) loads the image, makes a workspace (`init`), starts one
container per workspace (`up`, as the caller's uid on the host's network; refused while any
container labelled `org.siprig.workspace=<the workspace>` runs) and waits for the site, and
stops, restarts, logs, `exec`s and upgrades the postgres major (`upgrade-pg`).

## Proof

`test/minimal-pack/` is a throwaway pack: one package, one verb, a setup. With the base
built:

```
kit pack test/minimal-pack --tag <t> --base siprig:<tag>
docker run --rm --entrypoint cat minimal-rig:<t> /opt/siprig/image/wrapper > minirig
./minirig init <empty dir> && ./minirig -w <dir> up && ./minirig -w <dir> hello && ./minirig -w <dir> down
```

`up` answers with the site's URL, whose `/api/config` names the pack's title. The kit's
refusals and its build context are `ts/siprig/test/pack-image.test.ts` and `kit.test.ts`;
the image's scripts without an image, `image-git`, `siprig-in` and `wrapper.test.ts`.
