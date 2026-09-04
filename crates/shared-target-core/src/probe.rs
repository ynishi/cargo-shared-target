use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, Result};

/// Whether the blocks of files in `src` can be cloned into `at` rather than
/// copied.
///
/// Asked of the filesystems rather than of their names. btrfs and bcachefs
/// always answer yes and ext4 always no, but XFS answers by how it was made
/// (`reflink=1`, mkfs's default only since xfsprogs 5.1), and a container layer
/// or a network mount can differ from whatever the mount table suggests.
///
/// Asked of both ends together, because that is the operation. A destination
/// that can clone within itself says nothing about cloning into it from
/// somewhere else: btrfs clones freely between its subvolumes and not at all
/// across a mount point, and those two look identical from `at` alone.
pub fn supports_reflink(src: &Path, at: &Path) -> Result<bool> {
    let probe = at.join(".cargo-shared-target-probe");
    fs::create_dir_all(&probe).map_err(Error::io("creating a probe directory at", &probe))?;

    let answer = probe_inside(src, &probe);

    // Clearing the probe must not take the run down: by this line the answer is
    // already known, and a probe left behind is untidy rather than wrong.
    let _ = fs::remove_file(probe.join("a"));
    let _ = fs::remove_file(probe.join("b"));
    let _ = fs::remove_dir(&probe);

    answer
}

fn probe_inside(src: &Path, probe: &Path) -> Result<bool> {
    let destination = probe.join("b");

    // The clone opens its destination with `O_EXCL`, so a `b` left behind by a
    // run that died between the clone and the cleanup would come back as
    // `AlreadyExists` — indistinguishable, at `is_ok()`, from a filesystem that
    // cannot clone at all. The whole tree would then be copied for a reason
    // that is not true and that nothing would print.
    let _ = fs::remove_file(&destination);

    let subject = match first_file(src) {
        // A real file from the tree about to be seeded. Cloning is instant
        // whatever it weighs, and failing costs nothing either.
        Some(existing) => existing,
        // An empty target directory has nothing to ask about, so the question
        // becomes whether `at` can clone at all. 8 KiB rather than an empty
        // file: btrfs keeps a small enough file inline in its metadata, where
        // cloning is a different question from the one being asked.
        None => {
            let written = probe.join("a");
            let mut file = fs::File::create(&written)
                .map_err(Error::io("writing the probe file", &written))?;
            file.write_all(&[0u8; 8192])
                .map_err(Error::io("writing the probe file", &written))?;
            file.sync_all()
                .map_err(Error::io("writing the probe file", &written))?;
            written
        }
    };

    // Only the clone's own failure is an answer rather than an error; anything
    // that stopped the lines above has returned already.
    Ok(reflink_copy::reflink(&subject, &destination).is_ok())
}

fn first_file(src: &Path) -> Option<PathBuf> {
    WalkDir::new(src)
        .min_depth(1)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .find(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
}
