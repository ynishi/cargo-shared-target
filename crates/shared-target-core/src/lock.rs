//! Taking part in Cargo's build lock, so that a tree is not read while it is
//! being written.
//!
//! Cargo holds a lock for the length of a build. Reading a target directory
//! without it can capture an artifact and then, moments later, the fingerprint
//! written after it — a destination tree that is torn in exactly the way the
//! staging rename exists to prevent, and torn silently, because what Cargo
//! reads afterwards says fresh.
//!
//! What this locks is not a stable contract. The path and the mechanism are
//! Cargo's internals, so [`BuildLocks::held`] is reported rather than assumed:
//! holding nothing is a fact about this run, not a guarantee about it.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Cargo's lock file, one per profile directory.
const LOCK_FILE: &str = ".cargo-lock";

/// Locks held for as long as this value lives. Dropping it closes the
/// descriptors, which is what releases them.
pub struct BuildLocks {
    held: Vec<(PathBuf, File)>,
}

impl BuildLocks {
    /// The lock files being held. Empty means none were found — a target
    /// directory nothing has built in yet, a Cargo that has moved them, or a
    /// platform without the call. It does not mean the tree is quiet.
    pub fn held(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.held.iter().map(|(path, _)| path.as_path())
    }
}

/// Takes a shared lock on every profile lock under `src`.
///
/// Shared rather than exclusive: several of these may run at once, and what
/// must not overlap is this and a build. Refused rather than waited on — a
/// caller told that a build is running can decide, where one left blocking on
/// a `cargo build` that has forty crates to go cannot.
pub fn acquire(src: &Path) -> Result<BuildLocks> {
    let mut held = Vec::new();

    for path in lock_files(src)? {
        if let Some(file) = lock_shared(&path)? {
            held.push((path, file));
        }
    }

    Ok(BuildLocks { held })
}

/// Cargo puts its lock in the profile directory — `<target>/debug` — and one
/// level deeper when the build was given a target triple:
/// `<target>/x86_64-unknown-linux-gnu/debug`.
///
/// Looked for in those two places rather than by walking. A profile directory
/// holds the artifacts, so descending into one to search for a lock file would
/// mean listing tens of thousands of entries to find something that is not
/// there.
fn lock_files(src: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();

    for candidate in directories_in(src)? {
        let direct = candidate.join(LOCK_FILE);
        if direct.is_file() {
            found.push(direct);
            // A profile directory holds no further profile directories, and it
            // is the expensive one to list.
            continue;
        }

        for nested in directories_in(&candidate)? {
            let deeper = nested.join(LOCK_FILE);
            if deeper.is_file() {
                found.push(deeper);
            }
        }
    }

    Ok(found)
}

fn directories_in(path: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(path).map_err(Error::io("reading", path))?;
    let mut directories = Vec::new();

    for entry in entries {
        let entry = entry.map_err(Error::io("reading", path))?;
        if entry.path().is_dir() {
            directories.push(entry.path());
        }
    }

    Ok(directories)
}

#[cfg(unix)]
fn lock_shared(path: &Path) -> Result<Option<File>> {
    use rustix::fs::{FlockOperation, flock};

    let file = File::open(path).map_err(Error::io("opening the build lock", path))?;

    match flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(Error::SourceBusy {
            path: path.to_path_buf(),
        }),
        Err(errno) => Err(Error::Io {
            op: "locking",
            path: path.to_path_buf(),
            source: errno.into(),
        }),
    }
}

#[cfg(not(unix))]
fn lock_shared(_path: &Path) -> Result<Option<File>> {
    // Nothing is claimed here rather than something being claimed falsely. The
    // count of locks held stays at zero, and the caller reports that.
    Ok(None)
}
