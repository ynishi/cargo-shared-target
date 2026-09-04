use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use filetime::FileTime;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::{lock, probe};

/// Files this size and above under a `deps/` directory keep a shared inode
/// rather than a copy of their own.
///
/// What makes sharing safe there is not the size but what Cargo does with the
/// file: a compiled artifact under `deps/` is written once under a hashed name
/// and never modified, so two trees pointing at one inode never disagree. The
/// size is what makes it worth doing.
pub const DEFAULT_MIN_SHARED_SIZE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The filesystem clones blocks, so every file is shared and none of the
    /// tree costs disk until something writes to it.
    Clone,
    /// The filesystem does not clone, so the large write-once artifacts are
    /// shared by hard link and everything else is given a copy of its own.
    LinkAndCopy,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The target directory to seed from.
    pub src: PathBuf,
    /// The target directory to create.
    pub dest: PathBuf,
    /// Where to build the tree before it is renamed into place. Defaults to a
    /// sibling of `dest`.
    pub staging: Option<PathBuf>,
    /// See [`DEFAULT_MIN_SHARED_SIZE`].
    pub min_shared_size: u64,
}

impl Options {
    pub fn new(src: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        Self {
            src: src.into(),
            dest: dest.into(),
            staging: None,
            min_shared_size: DEFAULT_MIN_SHARED_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub strategy: Strategy,
    pub dest: PathBuf,
    pub dirs: u64,
    pub shared: u64,
    pub copied: u64,
    pub symlinks: u64,
    pub incremental_dropped: u64,
    pub shared_bytes: u64,
    pub copied_bytes: u64,
    /// How many of Cargo's build locks were held while the source was read.
    /// Zero is a fact about this run and not a guarantee about it — see
    /// [`crate::lock`].
    pub build_locks_held: usize,
}

/// Builds `dest` from `src`, sharing whatever the filesystem under `dest` lets
/// it share.
///
/// The tree is built under a staging name and renamed into place only once it
/// is whole. Cargo reads freshness from a fingerprint beside each artifact, so
/// a tree that is half there is worse than no tree at all: the fingerprints
/// that did land say fresh about artifacts that did not. Under the staging name
/// an unfinished tree is not a `target/` at all, and is inert rather than wrong.
pub fn seed(opts: &Options) -> Result<Report> {
    if !opts.src.is_dir() {
        return Err(Error::SourceMissing(opts.src.clone()));
    }
    if opts.dest.exists() {
        return Err(Error::DestinationExists(opts.dest.clone()));
    }

    let staging = opts
        .staging
        .clone()
        .unwrap_or_else(|| default_staging(&opts.dest));
    if staging.exists() {
        return Err(Error::StagingExists(staging));
    }

    // Before anything is created, including the parents. A refusal that has
    // already made a directory inside the tree it is refusing to write into is
    // not the promise this makes.
    refuse_nesting(&opts.src, &opts.dest, &staging)?;

    let dest_parent = parent_of(&opts.dest)?;
    fs::create_dir_all(dest_parent).map_err(Error::io("creating", dest_parent))?;
    let staging_parent = parent_of(&staging)?.to_path_buf();
    fs::create_dir_all(&staging_parent).map_err(Error::io("creating", &staging_parent))?;

    // The staged tree is renamed into place at the end, and a rename does not
    // cross a filesystem. This one holds whichever way the blocks get there,
    // and finding it out at the last line would mean discovering it after the
    // whole tree had been built.
    same_filesystem(&staging_parent, dest_parent)?;

    let strategy = if probe::supports_reflink(&opts.src, &staging_parent)? {
        Strategy::Clone
    } else {
        Strategy::LinkAndCopy
    };

    // Asked only of the strategy that needs it. A hard link cannot cross a
    // device, but a clone can: btrfs gives every subvolume a device number of
    // its own while `FICLONE` works across all of them, so refusing on that
    // number would turn away the arrangement with the most to gain — a worktree
    // in its own subvolume, where the seeding would have cost nothing at all.
    if strategy == Strategy::LinkAndCopy {
        same_filesystem(&opts.src, &staging_parent)?;
    }

    // Held until the reading is done. Taken after the probe, which writes only
    // into the staging parent and reads one file that a build would not be
    // rewriting under it.
    let locks = lock::acquire(&opts.src)?;

    // A failure leaves the staged tree where it is. It is named so that nothing
    // reads it, and deleting on the way out of an error is how the one run that
    // could have been inspected stops being inspectable.
    let mut report = fill(&opts.src, &staging, strategy, opts.min_shared_size)?;
    report.build_locks_held = locks.held().len();
    drop(locks);

    fs::rename(&staging, &opts.dest)
        .map_err(Error::io("renaming the staged tree to", &opts.dest))?;
    report.dest = opts.dest.clone();
    Ok(report)
}

fn fill(src: &Path, staging: &Path, strategy: Strategy, min_shared_size: u64) -> Result<Report> {
    fs::create_dir_all(staging).map_err(Error::io("creating", staging))?;

    let mut report = Report {
        strategy,
        dest: staging.to_path_buf(),
        dirs: 0,
        shared: 0,
        copied: 0,
        symlinks: 0,
        incremental_dropped: 0,
        shared_bytes: 0,
        copied_bytes: 0,
        build_locks_held: 0,
    };

    // Dropped rather than carried when carrying it would mean real bytes.
    // Cargo regenerates `incremental/`, it is the one large thing here that
    // nothing needs brought over, and under `LinkAndCopy` almost none of it is
    // large enough to be shared — so it would arrive as a copy and cost its
    // full size. Where blocks clone it costs nothing, so it comes along.
    let drop_incremental = strategy == Strategy::LinkAndCopy;
    let dropped = Cell::new(0u64);

    let walker = WalkDir::new(src)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            let prune = drop_incremental && is_profile_incremental(entry.path(), entry.file_type());
            if prune {
                dropped.set(dropped.get() + 1);
            }
            !prune
        });

    for entry in walker {
        let entry = entry.map_err(|source| Error::Walk {
            path: src.to_path_buf(),
            source,
        })?;
        let relative = entry
            .path()
            .strip_prefix(src)
            .expect("walkdir yields paths under its root");
        let destination = staging.join(relative);
        let file_type = entry.file_type();

        if file_type.is_dir() {
            fs::create_dir_all(&destination).map_err(Error::io("creating", &destination))?;
            report.dirs += 1;
            continue;
        }

        if file_type.is_symlink() {
            let points_to = fs::read_link(entry.path())
                .map_err(Error::io("reading the symlink", entry.path()))?;
            symlink(entry.path(), &points_to, &destination)?;
            report.symlinks += 1;
            continue;
        }

        let metadata = entry.metadata().map_err(|source| Error::Walk {
            path: entry.path().to_path_buf(),
            source,
        })?;
        let size = metadata.len();

        let share = match strategy {
            Strategy::Clone => true,
            Strategy::LinkAndCopy => {
                under_deps(relative) && !is_dep_info(relative) && size >= min_shared_size
            }
        };

        if share {
            match strategy {
                Strategy::Clone => {
                    reflink_copy::reflink(entry.path(), &destination)
                        .map_err(Error::io("cloning to", &destination))?;
                    carry_metadata(&metadata, &destination)?;
                }
                // A hard link is the same inode, so it arrives with the times
                // and the mode already on it.
                Strategy::LinkAndCopy => {
                    fs::hard_link(entry.path(), &destination)
                        .map_err(Error::io("linking to", &destination))?;
                }
            }
            report.shared += 1;
            report.shared_bytes += size;
        } else {
            fs::copy(entry.path(), &destination).map_err(Error::io("copying to", &destination))?;
            carry_metadata(&metadata, &destination)?;
            report.copied += 1;
            report.copied_bytes += size;
        }
    }

    report.incremental_dropped = dropped.get();
    Ok(report)
}

/// Carries the modification time and the mode across.
///
/// What the times are for is that the seeded tree should be the same tree. They
/// are not load-bearing for freshness in the direction it is tempting to
/// assume: Cargo compares each source file against the dep-info recorded beside
/// the artifact, so a copy landing with today's date is *newer* than what it was
/// built from and reads as fresh either way. Carrying them is what keeps the two
/// trees answering the same question the same way — and what keeps a hard link,
/// which has no choice but to bring its times along, from being the odd one out
/// beside the files that were copied.
fn carry_metadata(source: &fs::Metadata, destination: &Path) -> Result<()> {
    fs::set_permissions(destination, source.permissions())
        .map_err(Error::io("setting the mode on", destination))?;
    filetime::set_file_times(
        destination,
        FileTime::from_last_access_time(source),
        FileTime::from_last_modification_time(source),
    )
    .map_err(Error::io("setting the timestamps on", destination))?;
    Ok(())
}

/// Cargo's own incremental directory, told apart from any other directory that
/// happens to carry the name.
///
/// Matching on the name alone reaches into `OUT_DIR`, where a build script is
/// free to emit an `incremental/` of its own. Dropping that while carrying the
/// `run-build-script` fingerprint beside it produces exactly the tree the
/// staging rename exists to prevent — one whose fingerprints say fresh about
/// files that are not there — only arrived at from the other side, and Cargo
/// will not re-run the script that would put them back.
///
/// The one Cargo writes sits in a profile directory, and what distinguishes a
/// profile directory is that `deps/` is beside it.
fn is_profile_incremental(path: &Path, file_type: fs::FileType) -> bool {
    file_type.is_dir()
        && path.file_name() == Some(OsStr::new("incremental"))
        && path
            .parent()
            .is_some_and(|profile| profile.join("deps").is_dir())
}

/// Dep-info, which is never shared however large it grows.
///
/// Cargo rewrites these on every build, and what they contain is a list of
/// absolute source paths. Two checkouts sitting at two paths need two of them
/// saying two different things, so one inode between them is wrong on both
/// counts. The size rule below happens to keep them apart at the default
/// threshold, which is not the same as deciding to.
fn is_dep_info(relative: &Path) -> bool {
    relative
        .extension()
        .is_some_and(|extension| extension == "d")
}

/// `deps` as an ancestor rather than as the file's own name: what is being
/// asked is which directory Cargo writes the file into.
fn under_deps(relative: &Path) -> bool {
    relative.parent().is_some_and(|parent| {
        parent
            .components()
            .any(|component| component.as_os_str() == "deps")
    })
}

/// Refuses a destination that lives under the directory being read.
///
/// The walk and the writing would then be the same tree: every file written
/// becomes a file to walk, which becomes a file to write. Nothing about it
/// converges, and what it consumes is the disk. Both the destination and the
/// staged tree are checked, since the staged one is where everything is written
/// first.
///
/// Both paths are resolved through whatever symlinks their existing parts
/// contain before being compared — the directory a worktree is cut into is
/// free to be a symlink, and
/// comparing the paths as they were written would let a destination that
/// reaches the source by another name straight through.
fn refuse_nesting(src: &Path, dest: &Path, staging: &Path) -> Result<()> {
    let source = src.canonicalize().map_err(Error::io("resolving", src))?;

    for candidate in [dest, staging] {
        let resolved = resolve(candidate)?;
        if resolved.starts_with(&source) {
            return Err(Error::DestinationInsideSource {
                src: source,
                dest: resolved,
            });
        }
    }

    Ok(())
}

/// Resolves a path that does not exist yet, by resolving as much of it as does
/// and keeping the rest as written.
///
/// `canonicalize` needs the whole path to be there, and this runs before
/// anything has been created on purpose.
fn resolve(path: &Path) -> Result<PathBuf> {
    let mut unresolved = Vec::new();
    let mut existing = path.to_path_buf();

    loop {
        if let Ok(mut base) = existing.canonicalize() {
            base.extend(unresolved.iter().rev());
            return Ok(base);
        }

        let name = existing
            .file_name()
            .ok_or_else(|| Error::DestinationHasNoParent(path.to_path_buf()))?
            .to_os_string();
        unresolved.push(name);
        existing = parent_of(&existing)?.to_path_buf();
    }
}

fn parent_of(path: &Path) -> Result<&Path> {
    match path.parent() {
        // A relative single-component path has a parent, and it is empty.
        Some(parent) if parent.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(parent) => Ok(parent),
        None => Err(Error::DestinationHasNoParent(path.to_path_buf())),
    }
}

fn default_staging(dest: &Path) -> PathBuf {
    let name = dest.file_name().unwrap_or_else(|| OsStr::new("target"));
    let mut staged = name.to_os_string();
    staged.push(".partial");
    dest.with_file_name(staged)
}

#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let left = fs::metadata(a).map_err(Error::io("reading", a))?;
    let right = fs::metadata(b).map_err(Error::io("reading", b))?;
    if left.dev() == right.dev() {
        return Ok(());
    }
    Err(Error::CrossDevice {
        src: a.to_path_buf(),
        dest: b.to_path_buf(),
    })
}

#[cfg(not(unix))]
fn same_filesystem(_a: &Path, _b: &Path) -> Result<()> {
    // Left to the clone and the link to report in their own words rather than
    // guessed at from a device number this platform does not hand out.
    Ok(())
}

#[cfg(unix)]
fn symlink(_link: &Path, points_to: &Path, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(points_to, at).map_err(Error::io("creating the symlink", at))
}

#[cfg(not(unix))]
fn symlink(link: &Path, _points_to: &Path, at: &Path) -> Result<()> {
    // Followed rather than recreated: making a symlink is a privileged
    // operation on Windows, and a copy of what it pointed at is the same tree
    // to a compiler.
    //
    // Copied from the link itself rather than from what `read_link` returned.
    // That is normally relative to the link's own directory, and handing it to
    // `fs::copy` would resolve it against the working directory instead —
    // failing outright, or finding some unrelated file that happens to sit
    // there and writing its bytes into the tree as if they belonged.
    fs::copy(link, at)
        .map(|_| ())
        .map_err(Error::io("copying what the symlink pointed at, to", at))
}
