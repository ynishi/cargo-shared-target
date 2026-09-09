use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no target directory at {0}")]
    SourceMissing(PathBuf),

    #[error("{0} already exists")]
    DestinationExists(PathBuf),

    #[error(
        "a previous run left a partial tree at {0}; `cargo shared-target --prune <dir>` reports \
         what it holds and removes it"
    )]
    StagingExists(PathBuf),

    /// Two seedings pointed at one staging path. The marker is created inside a
    /// directory that did not exist a moment earlier, so somebody else holding
    /// its lock is not a leftover — it is a run in progress.
    #[error("another seeding is already building a tree at {0}")]
    StagingBusy(PathBuf),

    #[error("writing the staging marker at {path}")]
    MarkerWrite {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// Something wrote a file under the marker's name that this cannot read.
    /// Removing a tree on the strength of that is worse than declining to.
    #[error("the staging marker at {path} cannot be read")]
    MarkerUnreadable {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("the staging marker at {0} does not say what it was seeding")]
    MarkerIncomplete(PathBuf),

    #[error("no directory at {0} to look under")]
    PruneRootMissing(PathBuf),

    /// Reading a target directory a build is writing to yields a tree whose
    /// fingerprints and artifacts were captured at different moments — and
    /// Cargo reads that tree as fresh. Refused while the build holds its lock.
    #[error("a build is running here: {path} is locked; seed between builds")]
    SourceBusy { path: PathBuf },

    #[error("{0} has no parent directory to be created in")]
    DestinationHasNoParent(PathBuf),

    /// Walking the source while writing into it feeds the walk its own output,
    /// and what that fills is the disk. Refused rather than detected partway.
    #[error("{dest} is inside {src}, which is the directory being read")]
    DestinationInsideSource { src: PathBuf, dest: PathBuf },

    /// Neither a block clone nor a hard link crosses a filesystem boundary, and
    /// the copy that would remain is the multi-gigabyte one this tool exists to
    /// avoid. Said rather than silently done.
    #[error(
        "{src} and {dest} are on different filesystems, so there is nothing to share between them"
    )]
    CrossDevice { src: PathBuf, dest: PathBuf },

    #[error("{op} {path}")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("walking {path}")]
    Walk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },

    #[error("running `cargo metadata`")]
    MetadataSpawn(#[source] std::io::Error),

    #[error("`cargo metadata` failed: {0}")]
    MetadataFailed(String),

    #[error("reading the output of `cargo metadata`")]
    MetadataParse(#[source] serde_json::Error),

    #[error("`cargo metadata` reported no target directory")]
    MetadataNoTargetDir,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Builds an [`Error::Io`] closure for `map_err`, so the path and the verb
    /// travel with the failure instead of being lost at the call site.
    pub(crate) fn io(
        op: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Error {
        let path = path.into();
        move |source| Error::Io { op, path, source }
    }
}
