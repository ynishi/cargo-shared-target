//! What these assert is the shape of the seeded tree, not the mechanism that
//! produced it: whether the filesystem under the test can clone blocks is a
//! property of wherever `cargo test` happens to be running, so each test that
//! cares reads [`Strategy`] back out of the report and says what should hold
//! under the one that was actually used.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use shared_target_core::{Error, Options, Report, Strategy, seed};

mod common;

const BIG: usize = 2 * 1024 * 1024;
const SMALL: usize = 512 * 1024;
const LONG_AGO: Duration = Duration::from_secs(1_600_000_000);

/// A target directory with one of everything the seeding has to decide about.
fn fixture(root: &Path) -> std::io::Result<()> {
    let debug = root.join("debug");
    fs::create_dir_all(debug.join("deps"))?;
    fs::create_dir_all(debug.join("incremental/a-hash"))?;
    fs::create_dir_all(debug.join("build/pkg-out"))?;

    fs::write(debug.join("deps/libbig-1234.rlib"), vec![0u8; BIG])?;
    fs::write(debug.join("deps/small-5678.d"), vec![0u8; SMALL])?;
    fs::write(debug.join("incremental/a-hash/blob.bin"), vec![0u8; BIG])?;
    fs::write(debug.join("build/pkg-out/output"), b"built")?;

    // The fingerprint's own date is what a seeded tree has to arrive still
    // carrying, so it is set to something no copy would produce by accident.
    let stamp = filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH + LONG_AGO);
    for file in [
        debug.join("deps/libbig-1234.rlib"),
        debug.join("deps/small-5678.d"),
        debug.join("build/pkg-out/output"),
    ] {
        filetime::set_file_times(file, stamp, stamp)?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink("../deps/libbig-1234.rlib", debug.join("alias.rlib"))?;

    Ok(())
}

fn seeded() -> (tempfile::TempDir, Report) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let report = seed(&Options::new(&src, dir.path().join("wt/target"))).expect("the seeding");
    common::assert_expected_strategy(&report);
    (dir, report)
}

#[cfg(unix)]
fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).expect("the file").ino()
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path)
        .expect("the file")
        .modified()
        .expect("a mtime")
}

#[test]
#[cfg(unix)]
fn a_large_artifact_under_deps_is_shared_rather_than_copied() {
    let (dir, report) = seeded();
    let relative = "debug/deps/libbig-1234.rlib";
    let from = dir.path().join("src-target").join(relative);
    let to = dir.path().join("wt/target").join(relative);

    // Under either strategy this file costs no second copy: one shares the
    // inode outright, the other shares the blocks beneath two of them.
    match report.strategy {
        Strategy::LinkAndCopy => assert_eq!(inode(&from), inode(&to), "should be one inode"),
        Strategy::Clone => {
            assert_ne!(
                inode(&from),
                inode(&to),
                "a clone is a file of its own sharing blocks, not one inode under two names"
            );
            assert_eq!(
                fs::read(&to).unwrap().len(),
                BIG,
                "the clone is not the file"
            );
        }
    }
    assert_eq!(report.shared_bytes, BIG as u64);
}

/// The arrangement a device-number check gets backwards.
///
/// btrfs gives every subvolume a device number of its own, and clones between
/// them anyway. A worktree cut into its own subvolume is the case with the most
/// to gain — the whole tree for nothing — and refusing it on `st_dev` would turn
/// away exactly that.
///
/// Runs only where CI has provided the second device; there is no way to make
/// one from inside a test.
#[test]
fn a_destination_on_another_device_of_one_filesystem_is_still_seeded() {
    let Some(other) = common::other_device() else {
        return;
    };

    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let elsewhere = other.join(format!("seeded-{}", std::process::id()));
    let _ = fs::remove_dir_all(&elsewhere);

    let report = seed(&Options::new(&src, elsewhere.join("target")));
    let report = report.expect("seeding across a subvolume boundary should not be refused");
    assert_eq!(
        report.strategy,
        Strategy::Clone,
        "blocks clone across this boundary; the seeding did `{}`",
        common::name(report.strategy)
    );
    assert_eq!(
        fs::read(elsewhere.join("target/debug/deps/libbig-1234.rlib"))
            .expect("the cloned artifact")
            .len(),
        BIG
    );

    let _ = fs::remove_dir_all(&elsewhere);
}

#[test]
#[cfg(unix)]
fn a_file_cargo_will_write_to_gets_one_of_its_own() {
    let (dir, report) = seeded();
    if report.strategy != Strategy::LinkAndCopy {
        return; // Where blocks clone, writing to one tree already leaves the other alone.
    }

    // Small enough, and outside deps/, are the two ways a file lands as a copy.
    for relative in ["debug/deps/small-5678.d", "debug/build/pkg-out/output"] {
        let from = dir.path().join("src-target").join(relative);
        let to = dir.path().join("wt/target").join(relative);
        assert_ne!(inode(&from), inode(&to), "{relative} should not be shared");
    }
}

/// A hard link brings its times whether or not anyone asked, so a copy that
/// does not is a file that disagrees with its neighbours about when the tree
/// was built. Cargo tolerates the newer date; the two trees no longer matching
/// is the reason to carry it.
#[test]
fn a_copy_arrives_with_the_same_date_as_the_file_it_came_from() {
    let (dir, _) = seeded();
    let expected = SystemTime::UNIX_EPOCH + LONG_AGO;

    for relative in ["debug/deps/small-5678.d", "debug/build/pkg-out/output"] {
        assert_eq!(
            modified(&dir.path().join("wt/target").join(relative)),
            expected,
            "{relative} did not keep the date of the file it was copied from"
        );
    }
}

/// Dep-info names absolute source paths and is rewritten on every build, so two
/// checkouts need two of them. The default threshold keeps them apart by
/// accident of size; this asks for the rule itself.
#[test]
#[cfg(unix)]
fn dep_info_is_never_shared_however_low_the_threshold() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let mut options = Options::new(&src, dir.path().join("wt/target"));
    options.min_shared_size = 0;
    let report = seed(&options).expect("the seeding");
    if report.strategy != Strategy::LinkAndCopy {
        return;
    }

    let relative = "debug/deps/small-5678.d";
    assert_ne!(
        inode(&src.join(relative)),
        inode(&dir.path().join("wt/target").join(relative)),
        "dep-info was shared between two trees"
    );

    // The threshold really was lowered, or the assertion above says nothing.
    let rlib = "debug/deps/libbig-1234.rlib";
    assert_eq!(
        inode(&src.join(rlib)),
        inode(&dir.path().join("wt/target").join(rlib))
    );
}

/// Writing into the tree being read feeds the walk its own output. What it
/// fills is the disk, so it is refused up front rather than noticed partway.
#[test]
fn a_destination_inside_the_source_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let before = listing(&src);
    let attempt = seed(&Options::new(&src, src.join("nested/target")));
    assert!(
        matches!(attempt, Err(Error::DestinationInsideSource { .. })),
        "got {attempt:?}"
    );

    // Refusing to write into a tree, having already written into it, is not
    // what the refusal says it does.
    assert_eq!(
        listing(&src),
        before,
        "the refusal left something behind inside the source"
    );
}

/// Every path under `root`, for asserting that a tree is exactly as it was.
fn listing(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .collect();
    found.sort();
    found
}

/// The staged tree is where everything is written first, so a destination
/// safely outside the source is no help if staging was pointed back into it.
#[test]
fn a_staging_directory_inside_the_source_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let mut options = Options::new(&src, dir.path().join("wt/target"));
    options.staging = Some(src.join("partial"));
    let attempt = seed(&options);
    assert!(
        matches!(attempt, Err(Error::DestinationInsideSource { .. })),
        "got {attempt:?}"
    );
}

/// A build script may emit a directory called `incremental` into `OUT_DIR`, and
/// that one is its output rather than Cargo's cache. Dropping it while carrying
/// the `run-build-script` fingerprint beside it leaves Cargo certain the script
/// has already run and its output already there.
#[test]
fn a_build_script_may_emit_a_directory_of_that_name_and_keep_it() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let generated = src.join("debug/build/pkg-out/out/incremental");
    fs::create_dir_all(&generated).expect("the build script's output");
    fs::write(generated.join("table.rs"), b"pub const N: usize = 1;").expect("the generated file");
    fs::create_dir_all(src.join("debug/.fingerprint/pkg-abc")).expect("the fingerprint");
    fs::write(
        src.join("debug/.fingerprint/pkg-abc/run-build-script"),
        b"1",
    )
    .expect("the fingerprint");

    let report = seed(&Options::new(&src, dir.path().join("wt/target"))).expect("the seeding");
    let carried = dir
        .path()
        .join("wt/target/debug/build/pkg-out/out/incremental/table.rs");

    assert!(
        carried.exists(),
        "a build script's own output was dropped as if it were Cargo's cache"
    );
    if report.strategy == Strategy::LinkAndCopy {
        assert_eq!(
            report.incremental_dropped, 1,
            "only Cargo's own incremental directory should have been dropped"
        );
    }
}

/// The same refusal, reached by another name. Comparing the paths as they were
/// written would let this one through, and it is the ordinary case rather than
/// a contrived one: the directory a worktree is cut into is free to be a
/// symlink.
#[test]
#[cfg(unix)]
fn a_destination_reaching_the_source_through_a_symlink_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let by_another_name = dir.path().join("elsewhere");
    std::os::unix::fs::symlink(&src, &by_another_name).expect("the symlink");

    let attempt = seed(&Options::new(&src, by_another_name.join("nested/target")));
    assert!(
        matches!(attempt, Err(Error::DestinationInsideSource { .. })),
        "got {attempt:?}"
    );
}

/// Setting a mode and then a time on the file it was just applied to is an
/// order that only works one way round.
#[test]
#[cfg(unix)]
fn a_read_only_file_is_carried_without_complaint() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a temporary directory");
    let src = dir.path().join("src-target");
    fixture(&src).expect("the fixture");

    let locked = src.join("debug/build/pkg-out/output");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).expect("making it read-only");

    let report = seed(&Options::new(&src, dir.path().join("wt/target")));
    assert!(report.is_ok(), "got {report:?}");

    let carried = dir.path().join("wt/target/debug/build/pkg-out/output");
    assert_eq!(
        fs::metadata(&carried).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

#[test]
fn incremental_is_left_behind_when_bringing_it_would_cost_real_bytes() {
    let (dir, report) = seeded();
    let carried = dir.path().join("wt/target/debug/incremental").exists();

    match report.strategy {
        Strategy::LinkAndCopy => {
            assert!(!carried, "incremental would have been copied in full");
            assert_eq!(report.incremental_dropped, 1);
        }
        Strategy::Clone => assert!(carried, "cloning it costs nothing"),
    }
}

#[test]
#[cfg(unix)]
fn a_symlink_arrives_as_a_symlink() {
    let (dir, report) = seeded();
    let link = dir.path().join("wt/target/debug/alias.rlib");

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("../deps/libbig-1234.rlib")
    );
    assert_eq!(report.symlinks, 1);
}

#[test]
fn nothing_is_left_under_the_staging_name() {
    let (dir, _) = seeded();
    assert!(!dir.path().join("wt/target.partial").exists());
}

#[test]
fn an_existing_destination_is_refused_rather_than_merged_into() {
    let (dir, _) = seeded();
    let again = seed(&Options::new(
        dir.path().join("src-target"),
        dir.path().join("wt/target"),
    ));
    assert!(
        matches!(again, Err(Error::DestinationExists(_))),
        "got {again:?}"
    );
}

#[test]
fn a_source_that_is_not_there_is_named_as_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let attempt = seed(&Options::new(
        dir.path().join("absent"),
        dir.path().join("wt/target"),
    ));
    assert!(
        matches!(attempt, Err(Error::SourceMissing(_))),
        "got {attempt:?}"
    );
}
