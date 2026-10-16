//! The catalogues a product registers on the HA tooling search path
//! (ADR-0049 §7.7): every `metrics/*.json` of a content directory, in the
//! export format, read beside the shipped binaries' export.

use std::path::{Path, PathBuf};

use crate::contract::Contract;

/// The catalogue files of the content directory `dir`, sorted.
pub fn catalogue_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir.join("metrics")) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
}

/// The contract of `export` (the shipped binaries' catalogues) extended with
/// every catalogue file of the content directories `dirs`, in order.
pub fn contract(export: &str, dirs: &[PathBuf]) -> Result<Contract, String> {
    let mut contract = Contract::from_json(export)?;
    for dir in dirs {
        if !dir.is_dir() {
            return Err(format!("{}: content directory not found", dir.display()));
        }
        for file in catalogue_files(dir) {
            let json =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            contract.extend_json(&json, &file.display().to_string())?;
        }
    }
    Ok(contract)
}
