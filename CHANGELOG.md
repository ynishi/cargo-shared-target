# Changelog

## Unreleased

A staged tree that a run did not finish is no longer anonymous. It was already
left where it was, on purpose; what it was not was findable, so tens of
gigabytes could sit under a name only the run that died had known.

- Each staged tree carries a marker naming what it was seeded from, where it was
  going, and which process was writing it. It travels with the rename and comes
  off once the tree has landed.
- `cargo shared-target --prune <dir>` reports the staged trees under a directory
  and what removing them would actually free, which is not what `du` says: the
  artifacts shared with the tree they were seeded from cost nothing to delete.
  `--remove` is what takes them.
- A tree something is still writing is left alone, established by taking the
  marker's lock rather than by trusting a pid written down inside it.
- A run that fails now says where it left its tree, rather than mentioning it
  only if a later run happens to collide with it.
- Probe directories left by a run that died mid-probe are cleared too.

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
