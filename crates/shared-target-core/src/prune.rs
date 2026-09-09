//! Finding the staged trees that runs left behind, and removing them.
//!
//! Seeding builds under a staging name and renames into place, so a run that
//! dies leaves a tree that is inert — and that nothing afterwards knows about.
//! The tree is not small: it is the same tens of gigabytes the seeding exists
//! to avoid spending twice.
//!
//! What is looked for is the marker, not the name. `--staging` puts the tree
//! anywhere, and the default name is a suffix on the destination rather than a
//! promise, so a search by name finds whichever of them happened to keep it.
//! See [`crate::staging`].

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::probe::PROBE_DIR;
use crate::staging::{self, Claim, MARKER, Marker};

/// Cargo writes this at the root of a target directory. Finding one is how a
/// finished target directory is told apart from an ordinary directory, which
/// matters only for how long the search takes: descending into a real
/// `target/` means listing tens of thousands of entries that cannot be what is
/// being looked for.
const CACHE_TAG: &str = "CACHEDIR.TAG";

#[derive(Debug, Clone)]
pub struct Options {
    /// Where to look. Everything under it is searched.
    pub root: PathBuf,
    /// Whether to remove what is found, rather than only report it.
    pub remove: bool,
}

impl Options {
    /// Looks under `root` and removes nothing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            remove: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing holds the marker's lock, so no run is writing here.
    Abandoned,
    /// A seeding is running. Left alone whatever was asked for.
    Building,
    /// This platform cannot say. Left alone, for the same reason.
    Undecidable,
    /// The tree arrived: this is the destination the marker names, so the
    /// rename has happened and what stands here is a target directory Cargo may
    /// well be using. Only the marker is left over, and only the marker goes.
    Landed,
}

/// What a staged tree weighs, and how much of that weight is its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct Weight {
    pub files: u64,
    pub dirs: u64,
    /// Every byte in the tree, which is what `du` reports.
    pub total: u64,
    /// The bytes in files another tree also has a name for. Removing this tree
    /// frees none of them, which is why `du` is not the answer to how much
    /// removing it gets back.
    ///
    /// `None` where the platform does not say. Counted by names rather than by
    /// blocks, so on a filesystem that clones blocks — where a seeded file has
    /// a name of its own and no storage of its own — this reads zero and the
    /// figure left over is an upper bound rather than a measurement.
    pub shared: Option<u64>,
}

impl Weight {
    /// What removing the tree would actually give back, where that can be told.
    pub fn freeable(&self) -> Option<u64> {
        self.shared.map(|shared| self.total.saturating_sub(shared))
    }
}

#[derive(Debug, Clone)]
pub struct Tree {
    pub staging: PathBuf,
    pub marker: Marker,
    pub state: State,
    pub weight: Weight,
    pub removed: bool,
}

/// A probe directory left where the seeding asked the filesystem whether it can
/// clone. Kilobytes rather than gigabytes — it is here because an untracked
/// directory at the root of a worktree is in the way of the gates people run
/// there, which is the same reason `--staging` exists.
#[derive(Debug, Clone)]
pub struct Probe {
    pub path: PathBuf,
    pub removed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub trees: Vec<Tree>,
    pub probes: Vec<Probe>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.trees.is_empty() && self.probes.is_empty()
    }
}

/// Searches `root` for staged trees, and removes the abandoned ones when asked.
pub fn prune(opts: &Options) -> Result<Report> {
    if !opts.root.is_dir() {
        return Err(Error::PruneRootMissing(opts.root.clone()));
    }

    let (staged, probes) = search(&opts.root)?;

    // Collected before anything is acted on. Whether a probe directory is safe
    // to remove depends on whether a seeding is running beside it, and that is
    // not known until the whole search is done.
    let mut report = Report::default();
    for path in staged {
        if let Some(tree) = examine(&path, opts.remove)? {
            report.trees.push(tree);
        }
    }
    for path in probes {
        report.probes.push(clear(&path, opts.remove, &report.trees));
    }

    Ok(report)
}

/// Every staged tree and probe directory under `root`.
fn search(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut staged = Vec::new();
    let mut probes = Vec::new();

    let mut walk = WalkDir::new(root).into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry.map_err(|source| Error::Walk {
            path: root.to_path_buf(),
            source,
        })?;
        if !entry.file_type().is_dir() {
            continue;
        }

        let path = entry.path();
        if entry.depth() > 0 && path.file_name().is_some_and(|name| name == ".git") {
            walk.skip_current_dir();
            continue;
        }

        // Asked before the tag below, because a staged tree carries a copy of
        // the tag it was seeded from.
        if path.join(MARKER).is_file() {
            staged.push(path.to_path_buf());
            walk.skip_current_dir();
            continue;
        }

        if entry.depth() > 0 && path.file_name().is_some_and(|name| name == PROBE_DIR) {
            probes.push(path.to_path_buf());
            walk.skip_current_dir();
            continue;
        }

        // Never at depth zero: a root pointed straight at a target directory is
        // a caller asking about that directory, not one to be skipped whole.
        if entry.depth() > 0 && path.join(CACHE_TAG).is_file() {
            walk.skip_current_dir();
        }
    }

    Ok((staged, probes))
}

/// Weighs one staged tree, and removes it when it is abandoned and removal was
/// asked for.
///
/// `None` when the tree stopped being one while it was being looked at: a
/// seeding that finished in that moment renamed it into place, and what stands
/// here now is somebody's target directory rather than anything to report.
fn examine(staging: &Path, remove: bool) -> Result<Option<Tree>> {
    let path = staging.join(MARKER);
    let marker = staging::read(&path)?;

    // Asked before anything is weighed, and long before anything is removed: a
    // tree that has landed is a target directory, weighing it means walking
    // tens of thousands of entries for a number nobody can act on, and removing
    // it would be removing the very thing the seeding was for.
    let landed = is_same_directory(staging, &marker.dest);

    let weight = if landed {
        Weight::default()
    } else {
        weigh(staging)?
    };

    // The lock is held across the removal, so a seeding cannot start in this
    // tree while it is being taken apart.
    let (state, removed) = match staging::claim(&path)? {
        Claim::Building => (State::Building, false),
        Claim::Undecidable => (State::Undecidable, false),
        Claim::Finished => return Ok(None),
        Claim::Abandoned(held) if landed => {
            let removed = remove && fs::remove_file(&path).is_ok();
            drop(held);
            (State::Landed, removed)
        }
        Claim::Abandoned(held) => {
            let removed = if remove {
                fs::remove_dir_all(staging).map_err(Error::io("removing", staging))?;
                true
            } else {
                false
            };
            drop(held);
            (State::Abandoned, removed)
        }
    };

    Ok(Some(Tree {
        staging: staging.to_path_buf(),
        marker,
        state,
        weight,
        removed,
    }))
}

/// Whether two paths name one directory, asked of the filesystem rather than of
/// the strings: the tree is reached by whatever route the search took, and the
/// marker records the route the seeding was given.
fn is_same_directory(one: &Path, other: &Path) -> bool {
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => false,
    }
}

fn weigh(staging: &Path) -> Result<Weight> {
    let mut weight = Weight::default();
    #[cfg(unix)]
    let mut shared = 0u64;

    for entry in WalkDir::new(staging).min_depth(1) {
        let entry = entry.map_err(|source| Error::Walk {
            path: staging.to_path_buf(),
            source,
        })?;
        if entry.file_type().is_dir() {
            weight.dirs += 1;
            continue;
        }
        if entry.file_type().is_symlink() {
            continue;
        }

        let metadata = entry.metadata().map_err(|source| Error::Walk {
            path: entry.path().to_path_buf(),
            source,
        })?;
        weight.files += 1;
        weight.total += metadata.len();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                shared += metadata.len();
            }
        }
    }

    #[cfg(unix)]
    {
        weight.shared = Some(shared);
    }

    Ok(weight)
}

/// Removes a probe directory, when what it holds is what a probe leaves and
/// nothing is seeding beside it.
///
/// A probe is written and cleared within one call, so one still here belongs to
/// a run that is gone — unless a run is going on in the same place at this
/// moment, in which case its probe is the one thing here that is in use.
fn clear(path: &Path, remove: bool, trees: &[Tree]) -> Probe {
    let busy_neighbour = trees.iter().any(|tree| {
        matches!(tree.state, State::Building | State::Undecidable)
            && tree.staging.parent() == path.parent()
    });

    let removable = remove && !busy_neighbour && holds_only_probe_files(path);
    let removed = removable && fs::remove_dir_all(path).is_ok();
    Probe {
        path: path.to_path_buf(),
        removed,
    }
}

/// Whether a directory holds nothing but the two files a probe writes.
///
/// Checked rather than assumed from the name: the directory is the caller's
/// filesystem, and a name is not a licence to delete whatever is under it.
fn holds_only_probe_files(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };

    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        if name != "a" && name != "b" {
            return false;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            return false;
        }
    }

    true
}
