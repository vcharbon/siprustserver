"""The disallowed lists of the root clippy.toml only tighten (ADR-0051 §2):
against the base no entry is removed, and no entry that counts in both tiers
gains the `pure tier` reason that limits it to pure crates. Either would make
sites vanish from the pass and `--update` lower their counts."""
import tomllib

KEYS = ("disallowed-macros", "disallowed-methods", "disallowed-types")
PURE_ONLY = "pure tier"


def entries(text, path):
    """`{(key, path): reason}` of a clippy.toml's disallowed lists."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise ValueError(f"{path}: {e}") from e
    found = {}
    for key in KEYS:
        for entry in data.get(key, []):
            if isinstance(entry, str):
                found[(key, entry)] = ""
            else:
                found[(key, entry.get("path", ""))] = entry.get("reason", "")
    return found


def loosened(now, base, now_path, base_path):
    """What the lists `now` loosen against `base`, one line each."""
    bad = []
    for (key, path), reason in sorted(base.items()):
        if (key, path) not in now:
            bad.append(f"{now_path} drops `{path}` from {key}, which {base_path} lists")
        elif not reason.startswith(PURE_ONLY) and now[(key, path)].startswith(PURE_ONLY):
            bad.append(f"{now_path} narrows `{path}` in {key} to the pure tier; "
                       f"{base_path} counts it in both")
    return bad
