//! Seeding a Cargo target directory from an existing one.
//!
//! Cutting a git worktree, or any second checkout, leaves a build that starts
//! from nothing. Copying `target/` across is the obvious fix and the wrong one:
//! it is routinely tens of gigabytes. What this crate does instead is build the
//! new directory out of the old one's storage — a block clone where the
//! filesystem offers it, and a hard link for the large write-once artifacts
//! where it does not — so the second tree is warm without being a second copy.
//!
//! What belongs here is what Cargo knows: where the target directory is, which
//! of its contents may be shared, which must be a file of their own, and that a
//! half-built tree must never be visible under a name Cargo reads. Deciding
//! *where* the new tree goes is the caller's; a git worktree is one answer and
//! not a special one.

mod error;
mod metadata;
mod probe;
mod seed;

pub use error::{Error, Result};
pub use metadata::workspace_target_dir;
pub use probe::supports_reflink;

pub use seed::{DEFAULT_MIN_SHARED_SIZE, Options, Report, Strategy, seed};
