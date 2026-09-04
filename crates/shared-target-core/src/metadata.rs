use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// The target directory Cargo itself would use for the workspace containing
/// `cwd`.
///
/// Asked of Cargo rather than assumed to be `./target`: it moves with
/// `build.target-dir`, with `CARGO_TARGET_DIR`, and with which package in a
/// workspace the caller happens to be standing in.
pub fn workspace_target_dir(cwd: Option<&Path>) -> Result<PathBuf> {
    // `CARGO` is set when this runs as a subcommand, and naming the same
    // toolchain that invoked us is what keeps the answer consistent with the
    // build that follows.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let out = cmd.output().map_err(Error::MetadataSpawn)?;
    if !out.status.success() {
        return Err(Error::MetadataFailed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(Error::MetadataParse)?;
    doc.get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or(Error::MetadataNoTargetDir)
}
