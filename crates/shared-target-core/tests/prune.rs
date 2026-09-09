//! What these assert is that the expensive thing a run can leave behind is
//! found afterwards, and that the two directories it must never be confused
//! with — a tree something is still writing, and a tree that arrived — are left
//! where they are.

use std::fs;
use std::path::Path;

use shared_target_core::prune::{self, State};
use shared_target_core::staging::{self, MARKER};
use shared_target_core::{Options, seed};

mod common;

const BIG: usize = 2 * 1024 * 1024;
const SMALL: usize = 512 * 1024;

/// A target directory with enough in it for the seeding to have decisions to
/// make, and one file it will not be allowed to read.
fn fixture(root: &Path) -> std::io::Result<()> {
    let debug = root.join("debug");
    fs::create_dir_all(debug.join("deps"))?;
    fs::create_dir_all(debug.join("build/pkg-out"))?;
    fs::write(debug.join(".cargo-lock"), b"")?;
    fs::write(debug.join("deps/libbig-1234.rlib"), vec![0u8; BIG])?;
    fs::write(debug.join("deps/small-5678.d"), vec![0u8; SMALL])?;
    fs::write(debug.join("build/pkg-out/output"), b"built")?;
    Ok(())
}

/// Seeds a tree that cannot finish, and returns where the staged one was left.
///
/// A run is stopped partway by taking away the read on one of the files it has
/// to copy — the same shape as being killed, arrived at from inside the
/// process. `None` where the run went through anyway, which is what happens
/// when the tests are running as a user permissions do not apply to.
fn abandoned_tree(dir: &Path) -> Option<std::path::PathBuf> {
    let src = dir.join("src-target");
    fixture(&src).expect("the fixture");

    let unreadable = src.join("debug/build/pkg-out/output");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
            .expect("taking away the read");
    }
    if fs::read(&unreadable).is_ok() {
        return None;
    }

    let attempt = seed(&Options::new(&src, dir.join("wt/target")));
    assert!(attempt.is_err(), "the seeding should not have finished");

    let staging = dir.join("wt/target.partial");
    assert!(staging.is_dir(), "the staged tree should still be there");
    Some(staging)
}

fn looked_at(root: &Path) -> prune::Report {
    prune::prune(&prune::Options::new(root)).expect("the search")
}

fn swept(root: &Path) -> prune::Report {
    prune::prune(&prune::Options {
        root: root.to_path_buf(),
        remove: true,
    })
    .expect("the sweep")
}

/// The whole point. A run that stops partway leaves gigabytes somewhere with a
/// name nothing else knows, and this is what knows it.
#[test]
#[cfg(unix)]
fn a_tree_a_run_left_behind_is_found() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let Some(staging) = abandoned_tree(dir.path()) else {
        return;
    };

    let report = looked_at(dir.path());
    assert_eq!(report.trees.len(), 1, "{report:?}");

    let tree = &report.trees[0];
    assert_eq!(tree.staging, staging);
    assert_eq!(tree.state, State::Abandoned);
    assert_eq!(tree.marker.dest, dir.path().join("wt/target"));
    assert!(
        !tree.removed,
        "a search that was not asked to remove anything"
    );
    assert!(staging.is_dir(), "and did not");
}

#[test]
#[cfg(unix)]
fn asking_for_it_to_go_is_what_takes_it() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let Some(staging) = abandoned_tree(dir.path()) else {
        return;
    };

    let report = swept(dir.path());
    assert_eq!(report.trees.len(), 1, "{report:?}");
    assert!(report.trees[0].removed);
    assert!(!staging.exists(), "the tree should be gone");

    // And the source it was seeded from is not.
    assert!(
        dir.path()
            .join("src-target/debug/deps/libbig-1234.rlib")
            .is_file()
    );
}

/// What `du` says a tree weighs is not what removing it gives back: the files
/// it shares with the tree it was seeded from cost nothing to delete, because
/// deleting them removes a name and not the storage.
#[test]
#[cfg(unix)]
fn what_a_tree_shares_is_not_counted_as_room_to_be_had() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let elsewhere = dir.path().join("src-target/debug/deps");
    fs::create_dir_all(&elsewhere).expect("somewhere to link from");
    let shared_file = elsewhere.join("libbig-1234.rlib");
    fs::write(&shared_file, vec![0u8; BIG]).expect("the artifact");

    let staging = dir.path().join("wt/target.partial");
    let held = staging::mark(&staging, &elsewhere, &dir.path().join("wt/target"))
        .expect("marking the staged tree");
    fs::create_dir_all(staging.join("debug/deps")).expect("the profile");
    fs::hard_link(&shared_file, staging.join("debug/deps/libbig-1234.rlib"))
        .expect("sharing the artifact");
    fs::write(staging.join("debug/deps/small-5678.d"), vec![0u8; SMALL]).expect("a copy");
    drop(held);

    let report = looked_at(dir.path());
    let tree = report
        .trees
        .iter()
        .find(|tree| tree.staging == staging)
        .expect("the staged tree");

    assert_eq!(tree.weight.shared, Some(BIG as u64));
    assert_eq!(
        tree.weight.freeable(),
        // The marker is a file in the tree too, and it is small enough that
        // asserting a range says more than asserting a total.
        Some(tree.weight.total - BIG as u64)
    );
    assert!(
        tree.weight.freeable().unwrap() >= SMALL as u64,
        "the copy should be counted as room to be had"
    );
}

/// The one thing this must never do. A seeding in progress holds the marker,
/// and a tree being written to is not a tree to delete.
#[test]
#[cfg(unix)]
fn a_tree_something_is_still_writing_is_left_alone() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let staging = dir.path().join("wt/target.partial");
    let held = staging::mark(
        &staging,
        &dir.path().join("src-target"),
        &dir.path().join("wt/target"),
    )
    .expect("marking the staged tree");
    fs::write(staging.join("half-written"), b"...").expect("something under way");

    let report = swept(dir.path());
    assert_eq!(report.trees.len(), 1, "{report:?}");
    assert_eq!(report.trees[0].state, State::Building);
    assert!(!report.trees[0].removed);
    assert!(staging.join("half-written").is_file(), "left where it was");

    drop(held);
}

/// The other thing it must never do. A run killed between the rename and the
/// removal of its marker leaves the marker inside a real target directory, and
/// what goes is the marker.
#[test]
#[cfg(unix)]
fn a_marker_left_on_a_tree_that_arrived_costs_the_tree_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let dest = dir.path().join("wt/target");
    fs::create_dir_all(dest.join("debug/deps")).expect("a target directory");
    fs::write(dest.join("debug/deps/libbig-1234.rlib"), vec![0u8; BIG]).expect("an artifact");

    // The tree is at its destination and the marker is still on it: what a
    // process killed in that one moment leaves.
    let held = staging::mark(&dest, &dir.path().join("src-target"), &dest).expect("the marker");
    drop(held);

    let report = swept(dir.path());
    assert_eq!(report.trees.len(), 1, "{report:?}");
    assert_eq!(report.trees[0].state, State::Landed);
    assert!(
        report.trees[0].removed,
        "the marker should have been cleared"
    );
    assert!(!dest.join(MARKER).exists());
    assert!(
        dest.join("debug/deps/libbig-1234.rlib").is_file(),
        "the target directory itself must be untouched"
    );
}

#[test]
fn a_seeding_that_finished_leaves_nothing_to_find() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let report = seed(&Options::new(&src, dir.path().join("wt/target"))).expect("the seeding");
    common::assert_expected_strategy(&report);

    assert!(looked_at(dir.path()).is_empty(), "nothing was left behind");
}

/// The search does not descend into a target directory, which is what keeps it
/// from listing tens of thousands of entries that cannot be what it is looking
/// for. Said out loud here because it is also a limit: a staged tree put inside
/// one is not found.
#[test]
fn the_search_stops_at_a_target_directory() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let inside = dir.path().join("target/deep/target.partial");
    fs::create_dir_all(dir.path().join("target")).expect("a target directory");
    fs::write(
        dir.path().join("target/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172",
    )
    .expect("Cargo's tag");

    let held = staging::mark(&inside, &dir.path().join("src"), &dir.path().join("dest"))
        .expect("the marker");
    drop(held);

    assert!(looked_at(dir.path()).is_empty());
}

/// A probe is written and cleared within one call, so one still on the disk
/// belongs to a run that is gone. What is under it is checked all the same: a
/// name is not a licence to delete whatever is wearing it.
#[test]
fn a_probe_directory_goes_only_when_what_is_under_it_is_a_probe() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let left = dir.path().join("wt/.cargo-shared-target-probe");
    let occupied = dir.path().join("other/.cargo-shared-target-probe");
    fs::create_dir_all(&left).expect("the leftover probe");
    fs::create_dir_all(&occupied).expect("a directory wearing the name");
    fs::write(left.join("a"), vec![0u8; 8192]).expect("what a probe writes");
    fs::write(occupied.join("notes.txt"), b"somebody else's").expect("what it is not");

    let report = swept(dir.path());
    assert!(!left.exists(), "the leftover probe should be gone");
    assert!(
        occupied.join("notes.txt").is_file(),
        "and this one left alone"
    );
    assert_eq!(report.probes.len(), 2);
    assert_eq!(
        report.probes.iter().filter(|probe| probe.removed).count(),
        1
    );
}

#[test]
fn a_root_that_is_not_there_is_named_as_the_reason() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let attempt = prune::prune(&prune::Options::new(dir.path().join("absent")));
    assert!(
        matches!(attempt, Err(shared_target_core::Error::PruneRootMissing(_))),
        "got {attempt:?}"
    );
}
