use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Args, Parser};
use shared_target_core::prune::{self, State, Tree, Weight};
use shared_target_core::staging::MARKER;
use shared_target_core::{DEFAULT_MIN_SHARED_SIZE, Options, Report, Strategy};

/// Cargo passes its own name as the first argument when it dispatches a
/// subcommand, so the command line this parses is `cargo shared-target ...`
/// rather than `shared-target ...`.
#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum Cargo {
    SharedTarget(SharedTarget),
}

/// Seed a new target directory from an existing one.
#[derive(Args)]
#[command(version, about)]
struct SharedTarget {
    /// Target directory to seed from. Defaults to the one Cargo would use here.
    #[arg(long, value_name = "DIR")]
    src: Option<PathBuf>,

    /// Target directory to create. Must not already exist.
    #[arg(long, value_name = "DIR", required_unless_present = "prune")]
    dest: Option<PathBuf>,

    /// Build the tree here before renaming it into place.
    /// Defaults to a sibling of --dest named after it.
    #[arg(long, value_name = "DIR")]
    staging: Option<PathBuf>,

    /// Where blocks cannot be cloned, share files under deps/ of at least this
    /// size instead of copying them.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MIN_SHARED_SIZE)]
    min_shared_size: u64,

    /// Instead of seeding: report the staged trees under DIR that runs left
    /// behind, and what removing them would give back.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = ["src", "dest", "staging", "min_shared_size"],
    )]
    prune: Option<PathBuf>,

    /// Remove what --prune found, rather than only saying what is there.
    #[arg(long, requires = "prune")]
    remove: bool,
}

/// A failure, and the tree it may have left on the disk.
///
/// Carried rather than printed where it happens: the note belongs under the
/// error rather than above it, and only the caller knows when the error has
/// been said.
struct Failure {
    error: anyhow::Error,
    partial: Option<PathBuf>,
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            partial: None,
        }
    }
}

impl From<shared_target_core::Error> for Failure {
    fn from(error: shared_target_core::Error) -> Self {
        Self {
            error: error.into(),
            partial: None,
        }
    }
}

fn main() -> ExitCode {
    let Cargo::SharedTarget(args) = Cargo::parse();

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("error: {:#}", failure.error);
            if let Some(partial) = failure.partial {
                // The bytes are already on the disk by the time this prints,
                // and nothing else will ever mention them.
                eprintln!("note: a partial tree remains at {}", partial.display());
                eprintln!(
                    "note: `cargo shared-target --prune {}` says what it holds",
                    partial.parent().unwrap_or(&partial).display()
                );
            }
            ExitCode::FAILURE
        }
    }
}

fn run(args: SharedTarget) -> Result<(), Failure> {
    if let Some(root) = args.prune {
        let report = prune::prune(&prune::Options {
            root: root.clone(),
            remove: args.remove,
        })?;
        describe_prune(&root, &report, args.remove);
        return Ok(());
    }

    let src = match args.src {
        Some(src) => src,
        None => shared_target_core::workspace_target_dir(None)
            .context("finding the target directory for this workspace")?,
    };

    let options = Options {
        src,
        // Guaranteed by `required_unless_present`, which has already run.
        dest: args.dest.expect("a destination outside of --prune"),
        staging: args.staging,
        min_shared_size: args.min_shared_size,
    };
    let staging = shared_target_core::staging_path(&options);

    match shared_target_core::seed(&options) {
        Ok(report) => {
            describe(&report);
            Ok(())
        }
        // Only when something is actually there: most of the refusals happen
        // before the first directory is made, and pointing at a path that does
        // not exist would send the reader looking for it.
        Err(error) => Err(Failure {
            error: error.into(),
            partial: staging.is_dir().then_some(staging),
        }),
    }
}

fn describe(report: &Report) {
    let how = match report.strategy {
        Strategy::Clone => "cloning blocks",
        Strategy::LinkAndCopy => "linking what is safe to share, copying the rest",
    };
    println!("seeded {} by {how}", report.dest.display());
    println!(
        "  shared  {:>9} files  {:>10}",
        report.shared,
        bytes(report.shared_bytes)
    );
    println!(
        "  copied  {:>9} files  {:>10}",
        report.copied,
        bytes(report.copied_bytes)
    );
    println!("  dirs    {:>9}", report.dirs);
    // Said either way. Zero is what a target directory nothing has built in
    // looks like, and it is also what a Cargo that has moved its lock looks
    // like; the number is reported so the difference is the reader's to make.
    println!(
        "  locks   {:>9} of Cargo's build locks held while reading",
        report.build_locks_held
    );
    if report.symlinks > 0 {
        println!("  links   {:>9}", report.symlinks);
    }
    if report.incremental_dropped > 0 {
        println!(
            "  dropped {:>9} incremental directories (Cargo rebuilds these)",
            report.incremental_dropped
        );
    }
}

fn describe_prune(root: &Path, report: &prune::Report, remove: bool) {
    if report.is_empty() {
        println!("nothing left behind under {}", root.display());
        return;
    }

    for tree in &report.trees {
        describe_tree(tree, remove);
        println!();
    }

    for probe in &report.probes {
        println!(
            "probe        {}{}",
            probe.path.display(),
            if probe.removed { "  (removed)" } else { "" }
        );
    }

    describe_total(report, remove);
}

fn describe_tree(tree: &Tree, remove: bool) {
    // A landed tree is a target directory with a marker still on it, and
    // printing what it weighs would read as an offer to delete it.
    if tree.state == State::Landed {
        println!("stale marker {}", tree.staging.join(MARKER).display());
        println!("  for        a tree that arrived: this is a target directory in use");
        println!(
            "  status     {}",
            if tree.removed {
                "marker removed; the directory is untouched"
            } else {
                "left alone; pass --remove to clear the marker"
            }
        );
        return;
    }

    println!("staged tree  {}", tree.staging.display());
    println!("  from       {}", tree.marker.src.display());
    println!("  for        {}", tree.marker.dest.display());
    println!(
        "  written    by pid {} {}, cargo-shared-target {}",
        tree.marker.pid,
        ago(tree.marker.started_at),
        tree.marker.version
    );
    println!(
        "  holds      {} files  {}",
        tree.weight.files,
        bytes(tree.weight.total)
    );
    describe_weight(&tree.weight);

    let status = match (tree.state, tree.removed) {
        (State::Abandoned, true) => "removed".to_string(),
        (State::Abandoned, false) if remove => "left alone: removing it failed".to_string(),
        (State::Abandoned, false) => "left alone; pass --remove to take it".to_string(),
        (State::Building, _) => "left alone: a seeding is running here".to_string(),
        (State::Undecidable, _) => {
            "left alone: this platform cannot say whether a run is still going".to_string()
        }
        // Returned above.
        (State::Landed, _) => unreachable!("a landed tree is described on its own"),
    };
    println!("  status     {status}");
}

/// What removing the tree would give back, which is not what it weighs.
fn describe_weight(weight: &Weight) {
    let (Some(shared), Some(freeable)) = (weight.shared, weight.freeable()) else {
        println!("  frees      unknown: sharing cannot be accounted on this platform");
        return;
    };

    if shared > 0 {
        println!(
            "  shared     {} of that is a second name for a file another tree also has",
            bytes(shared)
        );
    }
    // Every byte of a cloned file has a name of its own, so on a filesystem
    // that clones blocks the line above reads zero and this one is an upper
    // bound. Said as `at most` rather than measured, because measuring it means
    // asking the filesystem which extents two files share.
    println!("  frees      at most {}", bytes(freeable));
}

fn describe_total(report: &prune::Report, remove: bool) {
    let abandoned: Vec<&Tree> = report
        .trees
        .iter()
        .filter(|tree| tree.state == State::Abandoned)
        .collect();
    if abandoned.is_empty() {
        return;
    }

    let freeable: Option<u64> = abandoned
        .iter()
        .map(|tree| tree.weight.freeable())
        .sum::<Option<u64>>();

    let verb = if remove { "freed" } else { "would free" };
    match freeable {
        Some(total) => println!(
            "{} abandoned {}, {verb} at most {}",
            abandoned.len(),
            plural(abandoned.len(), "tree"),
            bytes(total)
        ),
        None => println!(
            "{} abandoned {}",
            abandoned.len(),
            plural(abandoned.len(), "tree")
        ),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Roughly how long ago, which is all this is ever read for.
fn ago(at: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(at) else {
        return "just now".to_string();
    };

    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(60 * 60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    if elapsed < MINUTE {
        "moments ago".to_string()
    } else if elapsed < HOUR {
        format!("{} minutes ago", elapsed.as_secs() / MINUTE.as_secs())
    } else if elapsed < DAY {
        format!("{} hours ago", elapsed.as_secs() / HOUR.as_secs())
    } else {
        format!("{} days ago", elapsed.as_secs() / DAY.as_secs())
    }
}

fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = count as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
