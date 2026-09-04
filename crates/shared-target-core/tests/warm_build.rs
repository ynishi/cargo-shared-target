//! The property the rest of the crate exists to produce: a seeded target
//! directory is one Cargo builds out of rather than around.
//!
//! These drive the real thing — real packages, built by the real Cargo, and
//! asked afterwards what they had to compile. Nothing below asserts about the
//! shape of the seeded tree; `seed.rs` does that. What is asserted here is that
//! Cargo agrees.
//!
//! The subjects are dependency-free of anything off the disk, so no registry,
//! network, or lockfile resolution stands between the test and its answer.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use shared_target_core::{Options, seed};

mod common;

/// A library at a fixed location. It stands in for everything that does *not*
/// move when a worktree is cut: the registry, the vendor directory, anything
/// whose path is the same before and after.
fn dependency(at: &Path) -> io::Result<()> {
    fs::create_dir_all(at.join("src"))?;
    fs::write(
        at.join("Cargo.toml"),
        "[package]\nname = \"warm-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(
        at.join("src/lib.rs"),
        "pub fn greeting() -> &'static str { \"warm\" }\n",
    )?;
    Ok(())
}

/// A binary that uses it, written fresh wherever it is asked for — so a second
/// one is a checkout that has just appeared, with the modification times a
/// checkout that has just appeared carries.
fn application(at: &Path) -> io::Result<()> {
    fs::create_dir_all(at.join("src"))?;
    fs::write(
        at.join("Cargo.toml"),
        "[package]\nname = \"warm-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nwarm-dep = { path = \"../dep\" }\n",
    )?;
    fs::write(
        at.join("src/main.rs"),
        "fn main() { println!(\"{}\", warm_dep::greeting()); }\n",
    )?;
    Ok(())
}

fn build_into(project: &Path, target: &Path) -> String {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["build", "--offline"])
        // Set by the Cargo running this test; inheriting it would point the
        // build under test back at this workspace.
        .env_remove("CARGO_MANIFEST_DIR")
        .env("CARGO_TARGET_DIR", target)
        // What this output is read for is the `Compiling` lines, and CI sets
        // `CARGO_TERM_COLOR=always` — which wraps each one in escape codes and
        // puts a reset between the word and the space after it, so every match
        // silently stops matching and the build looks like it compiled nothing.
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(project)
        .output()
        .expect("cargo should be runnable from a test Cargo itself started");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "the build failed:\n{stderr}");
    stderr
}

/// Cargo says `Compiling <name> v<version>` for work it did and stays quiet
/// about what it reused, so these lines are the whole question.
fn compiled(build_output: &str) -> Vec<String> {
    build_output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Compiling "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// What cutting a worktree actually looks like, and the reason any of this is
/// worth doing.
///
/// The checkout is new, so its own sources are new and its own crates are built
/// again — nothing can prevent that, and nothing here claims to. What the
/// seeding buys is everything *underneath*: the dependencies, whose sources did
/// not move, are found already built. On a real project that is the overwhelming
/// majority of a cold build.
#[test]
fn a_new_checkout_finds_its_dependencies_already_built() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    dependency(&dir.path().join("dep")).expect("the dependency");

    let first = dir.path().join("app");
    application(&first).expect("the application");
    let built = dir.path().join("target-built");
    let cold = compiled(&build_into(&first, &built));
    assert!(
        cold.iter().any(|name| name == "warm-dep"),
        "the first build did not compile the dependency: {cold:?}"
    );

    let seeded = dir.path().join("target-seeded");
    common::assert_expected_strategy(&seed(&Options::new(&built, &seeded)).expect("the seeding"));

    // The second checkout. Same sources, new path, new modification times —
    // exactly what `git worktree add` leaves behind.
    let second = dir.path().join("app-elsewhere");
    application(&second).expect("the second checkout");

    let warm = compiled(&build_into(&second, &seeded));
    assert!(
        !warm.iter().any(|name| name == "warm-dep"),
        "the dependency was built again, so the seeding bought nothing: {warm:?}"
    );
    assert!(
        warm.iter().any(|name| name == "warm-app"),
        "the checkout's own crate should have been built: {warm:?}"
    );
}

/// The same tree, asked from where it was built rather than from somewhere new:
/// then there is nothing left to do at all.
#[test]
fn cargo_reuses_a_seeded_target_instead_of_building_again() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    dependency(&dir.path().join("dep")).expect("the dependency");
    let project = dir.path().join("app");
    application(&project).expect("the application");

    let built = dir.path().join("target-built");
    assert!(
        !compiled(&build_into(&project, &built)).is_empty(),
        "the first build had nothing to compile, so the rest of this proves nothing"
    );

    let seeded = dir.path().join("target-seeded");
    seed(&Options::new(&built, &seeded)).expect("the seeding");

    // The control. Cargo given an empty directory does compile, so the
    // assertion that follows is about the seeding rather than about this
    // package being trivially fresh under any circumstances.
    assert!(
        !compiled(&build_into(&project, &dir.path().join("target-empty"))).is_empty(),
        "a build into an empty target compiled nothing; the test cannot tell the cases apart"
    );

    assert!(
        compiled(&build_into(&project, &seeded)).is_empty(),
        "Cargo rebuilt from a seeded target that it should have found complete"
    );
}

/// A shared inode and a broken link are the same size on disk and the same
/// shape in a directory listing. Running it is the difference.
#[test]
fn what_the_seeded_tree_produces_actually_runs() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    dependency(&dir.path().join("dep")).expect("the dependency");
    let project = dir.path().join("app");
    application(&project).expect("the application");

    let built = dir.path().join("target-built");
    build_into(&project, &built);
    let seeded = dir.path().join("target-seeded");
    seed(&Options::new(&built, &seeded)).expect("the seeding");
    build_into(&project, &seeded);

    let binary = seeded.join("debug/warm-app");
    let output = Command::new(&binary)
        .output()
        .unwrap_or_else(|error| panic!("running {}: {error}", binary.display()));

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "warm");
}
