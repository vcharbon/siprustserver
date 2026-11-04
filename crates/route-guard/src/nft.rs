//! Writing a ruleset: the [`Writer`] seam the guard writes through, and
//! [`Nft`], which feeds the script to `nft -f -` (one netlink transaction).

use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Where a ruleset script goes, applied within `bound`. `Err` names why it
/// was not applied.
pub trait Writer {
    fn write(
        &self,
        script: String,
        bound: Duration,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// `program -f -`, the script on its standard input, killed past the bound.
#[derive(Debug, Clone)]
pub struct Nft {
    pub program: String,
}

impl Writer for Nft {
    async fn write(&self, script: String, bound: Duration) -> Result<(), String> {
        match tokio::time::timeout(bound, self.run(script)).await {
            Ok(result) => result,
            Err(_) => Err(format!("{} -f - ran past {} ms", self.program, bound.as_millis())),
        }
    }
}

impl Nft {
    async fn run(&self, script: String) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{} cannot start: {e}", self.program))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(script.as_bytes())
                .await
                .map_err(|e| format!("{} took no script: {e}", self.program))?;
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("{} did not end: {e}", self.program))?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!("{} -f - failed ({}): {}", self.program, out.status, stderr.trim()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUND: Duration = Duration::from_secs(5);

    /// A stand-in for `nft` that copies its standard input to `out`.
    fn recorder(dir: &std::path::Path) -> (String, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let out = dir.join("script.nft");
        let program = dir.join("nft");
        std::fs::write(&program, format!("#!/bin/sh\ncat > '{}'\n", out.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (program.display().to_string(), out)
    }

    #[tokio::test]
    async fn the_script_reaches_the_programs_standard_input() {
        let dir = std::env::temp_dir().join(format!("route-guard-nft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (program, out) = recorder(&dir);
        let nft = Nft { program };
        assert_eq!(nft.write("table inet x\n".to_string(), BOUND).await, Ok(()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "table inet x\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_failing_or_missing_program_is_an_error_naming_it() {
        let failing = Nft { program: "false".to_string() };
        let e = failing.write(String::new(), BOUND).await.unwrap_err();
        assert!(e.starts_with("false -f - failed"), "{e}");
        let missing = Nft { program: "route-guard-no-such-nft".to_string() };
        let e = missing.write(String::new(), BOUND).await.unwrap_err();
        assert!(e.contains("cannot start"), "{e}");
    }
}
