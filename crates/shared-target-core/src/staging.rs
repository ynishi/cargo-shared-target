//! The note a staged tree carries while it is being built.
//!
//! What makes an interrupted seeding harmless is that the staged tree is not
//! named `target`: Cargo never reads it, so a tree that is half there is inert
//! rather than wrong. What makes it invisible afterwards is the same fact. The
//! name is the caller's to choose — `--staging` puts it anywhere — so there is
//! no name to look for later, and tens of gigabytes can sit somewhere with
//! nothing that outlived the run knowing they are there.
//!
//! So the tree says what it is from the inside. The marker is written before
//! the first file, travels with the rename, and is removed once the tree has
//! landed — so an abandoned tree always holds one, and a finished target
//! directory holds one only for the moment between the rename and the removal.
//! Which of the two a marker is in is not guessed at: it records where the tree
//! was going, and a marker sitting in that very directory is one the rename has
//! already carried home.
//!
//! It also says whether the run that made it is still going, and says it the
//! way Cargo does: by holding a lock rather than by writing down a fact that
//! then goes stale. A process that is killed releases its locks. A process that
//! wrote `running: true` and was killed did not, and the tree it left would be
//! unprunable forever.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// The marker's name, at the root of the staged tree.
///
/// Inside rather than beside it: a marker beside the tree is a second thing to
/// leave behind, and one that a `--staging` pointed elsewhere would strand in a
/// different directory from the tree it describes.
pub const MARKER: &str = ".cargo-shared-target-staging";

/// What a staged tree says about the run that was building it.
#[derive(Debug, Clone)]
pub struct Marker {
    /// The target directory it was being seeded from.
    pub src: PathBuf,
    /// Where it was going to be renamed to.
    pub dest: PathBuf,
    /// The process that was writing it. Reported rather than tested — see
    /// [`claim`], which asks the kernel instead.
    pub pid: u32,
    pub started_at: SystemTime,
    /// The version of this crate that wrote it.
    pub version: String,
}

/// A marker that exists and is locked for as long as this value lives.
///
/// Dropping it closes the descriptor, which is what releases the lock.
#[derive(Debug)]
pub struct Held {
    path: PathBuf,
    _file: File,
}

impl Held {
    /// The marker file this is holding.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the marker from `tree`, which is what turns a staged tree into a
    /// finished one.
    ///
    /// Taken from wherever the tree has ended up rather than from where it was
    /// written, because the marker travels with the rename. Removing it first
    /// and renaming afterwards would look tidier and leaves the one hole this
    /// exists to close: a rename that fails then leaves a tree with nothing in
    /// it to say what it is, and nothing will ever find it again.
    ///
    /// A marker already gone is not a failure. A prune running at this moment
    /// sees a marker sitting in the directory it names as its own destination,
    /// knows the tree has landed, and clears it — which is this, arrived at
    /// from the other side.
    pub fn release(self, tree: &Path) -> Result<()> {
        let path = tree.join(MARKER);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io("removing the staging marker", &path)(error)),
        }
    }
}

/// What holding the lock on a marker found.
#[derive(Debug)]
pub enum Claim {
    /// A seeding is running: something else holds the lock.
    Building,
    /// Nothing holds the lock. Whatever was writing this tree is gone, and the
    /// lock stays held for as long as the returned value lives — so that a
    /// seeding cannot start here while the tree is being removed.
    Abandoned(Held),
    /// The marker was gone by the time the lock was taken: the run finished and
    /// renamed its tree into place in between. There is nothing here to prune,
    /// and what stands at the path now is a real target directory.
    Finished,
    /// Whether a run is still going could not be established on this platform.
    /// Nothing is removed on the strength of a guess.
    Undecidable,
}

/// Writes the marker into `staging` and takes the lock on it.
///
/// Called by the seeding before it writes anything, and public because the
/// staging is the caller's to arrange: a caller filling a tree its own way owes
/// the same note to whoever comes looking afterwards.
pub fn mark(staging: &Path, src: &Path, dest: &Path) -> Result<Held> {
    fs::create_dir_all(staging).map_err(Error::io("creating", staging))?;
    let path = staging.join(MARKER);

    let body = serde_json::json!({
        "version": 1,
        "tool_version": env!("CARGO_PKG_VERSION"),
        "src": src.to_string_lossy(),
        "dest": dest.to_string_lossy(),
        "pid": std::process::id(),
        "started_at": seconds_since_epoch(SystemTime::now()),
    });

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .map_err(Error::io("writing the staging marker", &path))?;
    serde_json::to_writer_pretty(&mut file, &body).map_err(|source| Error::MarkerWrite {
        path: path.clone(),
        source,
    })?;

    lock_exclusive(&file, &path)?;
    Ok(Held { path, _file: file })
}

/// Reads what a marker says.
///
/// A marker that cannot be parsed is an error rather than a reason to treat the
/// tree as ordinary: something wrote a file by that name, and deleting a tree
/// on the strength of a file nobody can read is worse than declining to.
pub fn read(path: &Path) -> Result<Marker> {
    let text = fs::read(path).map_err(Error::io("reading the staging marker", path))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&text).map_err(|source| Error::MarkerUnreadable {
            path: path.to_path_buf(),
            source,
        })?;

    let string = |key: &str| doc.get(key).and_then(serde_json::Value::as_str);
    let (Some(src), Some(dest)) = (string("src"), string("dest")) else {
        return Err(Error::MarkerIncomplete(path.to_path_buf()));
    };

    Ok(Marker {
        src: PathBuf::from(src),
        dest: PathBuf::from(dest),
        pid: doc
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        started_at: UNIX_EPOCH
            + Duration::from_secs(
                doc.get("started_at")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            ),
        version: string("tool_version").unwrap_or("unknown").to_string(),
    })
}

/// Asks whether the run that made this tree is still going, by trying to take
/// the lock it would be holding.
///
/// The lock is kept when the answer is that nothing holds it, because the gap
/// between asking and acting is the whole problem: a seeding that finishes in
/// that gap renames its tree into place, and a caller acting on a stale answer
/// would delete a target directory that Cargo had just been handed.
#[cfg(unix)]
pub fn claim(path: &Path) -> Result<Claim> {
    use std::os::unix::fs::MetadataExt;

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Claim::Finished),
        Err(error) => return Err(Error::io("opening the staging marker", path)(error)),
    };

    match try_lock_exclusive(&file) {
        Locked::Busy => return Ok(Claim::Building),
        Locked::Taken => {}
        Locked::Failed(source) => {
            return Err(Error::Io {
                op: "locking",
                path: path.to_path_buf(),
                source,
            });
        }
    }

    // Taking the lock says nothing about what it is a lock *on*. A seeding that
    // finished between the open above and the line before this one has removed
    // this marker and renamed its tree away: the descriptor still refers to the
    // file, which now has no name, and locking an unlinked inode succeeds every
    // time. So the file is asked whether it is still the one at that path.
    let held = file
        .metadata()
        .map_err(Error::io("reading the staging marker", path))?;
    match fs::metadata(path) {
        Ok(current) if current.ino() == held.ino() && current.dev() == held.dev() => {
            Ok(Claim::Abandoned(Held {
                path: path.to_path_buf(),
                _file: file,
            }))
        }
        Ok(_) => Ok(Claim::Finished),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Claim::Finished),
        Err(error) => Err(Error::io("reading the staging marker", path)(error)),
    }
}

#[cfg(not(unix))]
pub fn claim(path: &Path) -> Result<Claim> {
    if !path.is_file() {
        return Ok(Claim::Finished);
    }
    // Nothing is claimed here rather than something being claimed falsely. A
    // tree reported as undecidable is left alone, which is the answer that
    // cannot delete a running build's work.
    Ok(Claim::Undecidable)
}

#[cfg(unix)]
enum Locked {
    Taken,
    Busy,
    Failed(std::io::Error),
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Locked {
    use rustix::fs::{FlockOperation, flock};

    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Locked::Taken,
        Err(rustix::io::Errno::WOULDBLOCK) => Locked::Busy,
        Err(errno) => Locked::Failed(errno.into()),
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File, path: &Path) -> Result<()> {
    match try_lock_exclusive(file) {
        Locked::Taken => Ok(()),
        // The marker was created inside a staging directory that did not exist
        // a moment ago, so something else holding its lock means two seedings
        // were pointed at one staging path.
        Locked::Busy => Err(Error::StagingBusy(path.to_path_buf())),
        Locked::Failed(source) => Err(Error::Io {
            op: "locking",
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File, _path: &Path) -> Result<()> {
    Ok(())
}

fn seconds_since_epoch(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}
