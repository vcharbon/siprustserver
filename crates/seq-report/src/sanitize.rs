//! Filesystem-safe artifact naming, shared by the write-on-Drop report
//! writers (`scenario-harness`'s `ArtifactDump`, `failover-harness`'s
//! `write_report_on_drop`).

/// Sanitize a scenario name into a single filesystem path segment: keep
/// `[A-Za-z0-9._-]`, fold everything else to `-`, collapse runs, and fall back
/// to `report` if the result is empty.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "report".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn names_fold_to_one_path_segment() {
        assert_eq!(sanitize_name("alice calls bob / take 2"), "alice-calls-bob-take-2");
        assert_eq!(sanitize_name("ok_name-1.2"), "ok_name-1.2");
        assert_eq!(sanitize_name("///"), "report");
    }
}
