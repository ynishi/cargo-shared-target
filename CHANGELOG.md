# Changelog

## 0.1.0

First release.

Seeds a new Cargo target directory from an existing one, so a second checkout
starts warm without being a second copy. Where the filesystem clones blocks it
clones everything; where it does not, the large write-once artifacts under
`deps/` are shared by hard link and the rest is copied.

- `cargo shared-target --dest <dir>` seeds from the target directory Cargo would
  use where you are standing, or from `--src` when given.
- Takes a share of Cargo's build lock while reading, and refuses rather than
  reads a tree a build is writing to.
- Builds under a staging name and renames into place only once whole, so a run
  that fails leaves something inert rather than a `target/` whose fingerprints
  disagree with its artifacts.
