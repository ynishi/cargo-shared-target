//! What the tests need from the environment they are running in.
//!
//! Half of this crate only runs on a filesystem that can clone blocks, and the
//! machine most of the writing happens on cannot. A suite that quietly takes
//! the other branch there — and passes — is a suite that says nothing about the
//! half it did not reach, which is the half with no other way of being checked.
//!
//! So the environment states which branch it is supposed to produce, and a run
//! that produces the other one fails rather than skips.

#![allow(dead_code)]

use std::path::PathBuf;

use shared_target_core::{Report, Strategy};

/// Set by CI to the strategy the filesystem under `TMPDIR` should yield:
/// `clone` on btrfs or on XFS made with `reflink=1`, `link-and-copy` on ext4.
/// Unset locally, where either is fine.
const EXPECTED: &str = "CARGO_SHARED_TARGET_EXPECT_STRATEGY";

/// Set by CI to a directory on a *different device* that shares a filesystem
/// with `TMPDIR` — a second btrfs subvolume. Blocks clone across that boundary
/// and hard links do not, which is the one case a device-number check gets
/// backwards.
const OTHER_DEVICE: &str = "CARGO_SHARED_TARGET_OTHER_DEVICE_DIR";

pub fn name(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::Clone => "clone",
        Strategy::LinkAndCopy => "link-and-copy",
    }
}

/// Fails when the run took a branch the environment said it would not.
pub fn assert_expected_strategy(report: &Report) {
    let Some(expected) = std::env::var_os(EXPECTED) else {
        return;
    };
    let expected = expected.to_string_lossy().to_ascii_lowercase();
    let actual = name(report.strategy);

    assert_eq!(
        actual, expected,
        "{EXPECTED} asked for `{expected}` and the seeding did `{actual}`; \
         the filesystem under TMPDIR is not the one this job meant to test"
    );
}

pub fn other_device() -> Option<PathBuf> {
    std::env::var_os(OTHER_DEVICE).map(PathBuf::from)
}
