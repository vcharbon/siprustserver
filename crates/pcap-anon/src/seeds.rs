//! The kept seed of a capture. The seed inverts the fake
//! mapping by dictionary, so it is kept in a file the caller names (the
//! ingest keeps it in the store's owner-only seed directory), owner-readable
//! only, read from that file alone, and never printed nor written inside the
//! anonymised output. A capture anonymised again under its kept seed comes out
//! byte-identical and keeps its name; a fresh seed is a new capture, so a
//! fresh seed is refused where one is already kept, and a run refuses to draw
//! one nobody keeps.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use crate::fake::Seed;

/// The run's seed from the command line's `--seed-file <path>` and
/// `--fresh-seed`: a kept file is read — refused when anyone but its owner
/// may read it — and `--fresh-seed` beside one is refused; a missing file takes `--fresh-seed`, which draws a seed and keeps
/// it there with mode 600; anything else is refused.
pub fn resolve(seed_file: Option<&Path>, fresh: bool) -> Result<Seed, String> {
    let Some(path) = seed_file else {
        return Err(if fresh {
            "--fresh-seed needs --seed-file: a seed nobody keeps makes the capture irreproducible"
        } else {
            "no seed: pass --seed-file (with --fresh-seed for a new capture)"
        }
        .to_string());
    };
    if path.exists() {
        if fresh {
            return Err(format!(
                "a seed is kept at {}: a fresh seed is a new capture name, never this one",
                path.display()
            ));
        }
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("cannot read the kept seed {}: {e}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "the kept seed {} is readable beyond its owner (mode {:o}): chmod 600 it",
                path.display(),
                mode & 0o777
            ));
        }
        return std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the kept seed {}: {e}", path.display()))
            .and_then(|text| Seed::from_hex(&text))
            .map_err(|e| format!("{}: {e}", path.display()));
    }
    if !fresh {
        return Err(format!(
            "no seed is kept at {}: pass --fresh-seed for a new capture, or restore the seed",
            path.display()
        ));
    }
    let seed = Seed::random()?;
    keep(&seed, path)?;
    Ok(seed)
}

/// Write `seed` to `path`, owner-readable only, never over an existing file
/// and never as a partial one: the bytes go to a temporary file beside it,
/// which is then linked under the final name (refused if that name exists).
fn keep(seed: &Seed, path: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot keep the seed at {}: {e}", path.display());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| {
            writeln!(file, "{}", seed.to_hex())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::hard_link(&temp, path));
    let _ = std::fs::remove_file(&temp);
    written.map_err(fail)
}

#[cfg(test)]
mod tests;
