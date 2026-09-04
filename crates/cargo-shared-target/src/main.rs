use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser};
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
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// Build the tree here before renaming it into place.
    /// Defaults to a sibling of --dest named after it.
    #[arg(long, value_name = "DIR")]
    staging: Option<PathBuf>,

    /// Where blocks cannot be cloned, share files under deps/ of at least this
    /// size instead of copying them.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MIN_SHARED_SIZE)]
    min_shared_size: u64,
}

fn main() -> ExitCode {
    let Cargo::SharedTarget(args) = Cargo::parse();

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: SharedTarget) -> Result<()> {
    let src = match args.src {
        Some(src) => src,
        None => shared_target_core::workspace_target_dir(None)
            .context("finding the target directory for this workspace")?,
    };

    let options = Options {
        src,
        dest: args.dest,
        staging: args.staging,
        min_shared_size: args.min_shared_size,
    };

    describe(&shared_target_core::seed(&options)?);
    Ok(())
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
