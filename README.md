# cargo-shared-target

Seed a new Cargo target directory from an existing one.

Cutting a git worktree — or any second checkout — leaves a build that starts
from nothing. Copying `target/` across is the obvious fix and the wrong one: it
is routinely tens of gigabytes. This builds the new directory out of the old
one's storage instead, so the second tree is warm without being a second copy.

## What it shares

The filesystem decides which of the two it can do, and it is asked by cloning
one real 8 KiB file rather than by reading its name — XFS answers by how it was
made, and a container layer or a network mount can differ from what the mount
table suggests.

| | where blocks clone (btrfs, bcachefs, XFS with `reflink=1`, APFS) | where they do not (ext4) |
|---|---|---|
| everything | cloned; costs nothing until written to | — |
| large artifacts under `deps/` | — | one inode, shared |
| everything else | — | a copy of its own |
| `incremental/` | carried, since it is free | dropped; Cargo rebuilds it |

Sharing an artifact under `deps/` is safe because of what Cargo does with it,
not because of its size: it is written once under a hashed name and never
modified, so two trees pointing at one inode cannot disagree. The size is what
makes it worth doing. Everything Cargo may write to gets a file of its own.

Dep-info (`*.d`) is never shared however large it grows: Cargo rewrites it on
every build, and what it holds is a list of absolute source paths, so two
checkouts need two of them.

Copies keep their modification time. Not because Cargo would otherwise call them
stale — it compares each source against the dep-info beside the artifact, and a
copy landing with today's date is newer than what it was built from either way —
but because a hard link brings its times along whether or not anyone asked, and a
tree where half the files agree about when it was built and half do not is a
tree that answers questions differently from the one it came from.

The tree is built under `<dest>.partial` and renamed into place only once it is
whole. A half-built `target/` is worse than none: the fingerprints that landed
say fresh about artifacts that did not. Under the staging name it is not a
`target/` at all, so Cargo never reads it, and a run that fails leaves something
inert rather than something wrong — and findable, which is the other half of it:
see [When a run does not finish](#when-a-run-does-not-finish).

## Install

```bash
cargo install --path crates/cargo-shared-target
```

## Use

```bash
cargo shared-target --dest ./somewhere/target
```

`--src` defaults to the target directory Cargo would use where you are standing,
asked of `cargo metadata` rather than assumed to be `./target`.

```
--src DIR              target directory to seed from
--dest DIR             target directory to create; must not already exist
--staging DIR          build here before renaming into place
--min-shared-size N    share files under deps/ at least this large (default 1 MiB)

--prune DIR            instead of seeding: report the trees left under DIR by
                       runs that did not finish
--remove               remove what --prune found
```

Where the new tree goes is the caller's business. A git worktree is one answer
and not a special one, which is why there is no subcommand for it — `git` already
has the one line:

```just
worktree-new slug:
    git worktree add ../{{slug}} -b feat/{{slug}} origin/main
    cargo shared-target --dest ../{{slug}}/target
```

On a filesystem that cannot clone, the second line is minutes rather than
seconds, and nothing reads `target/` until something compiles — so it is worth
backgrounding it and letting the reading-the-issue half of the work run through
it.

Use `--staging` when an untracked directory at the worktree root would be in the
way of your own gates:

```just
    cargo shared-target --dest ../{{slug}}/target \
                        --staging ../{{slug}}/.staging/target.partial
```

## While a build is running

It isn't. Cargo holds a lock in each profile directory for the length of a
build, and this takes a share of every one it finds before reading anything —
so a seeding that overlaps a build is refused rather than performed:

```
error: a build is running here: /path/target/debug/.cargo-lock is locked; seed between builds
```

Refused rather than waited on: a caller told a build is running can decide, and
one left blocking on a build with forty crates to go cannot. The share is
released as soon as the reading is done, well before the rename.

What is being taken part in here is Cargo's own arrangement, not a published
one — the path and the mechanism are its internals. So the count of locks held
is reported rather than assumed:

```
  locks           2 of Cargo's build locks held while reading
```

Zero is what a target directory nothing has built in looks like. It is also what
a Cargo that has moved its lock would look like. The number is printed so the
difference stays the reader's to make.

## When a run does not finish

The staged tree stays where it is. That is deliberate — a run that could have
been looked at stops being one the moment its evidence is deleted on the way out
— but the tree is the same tens of gigabytes the whole exercise is about, and a
run that was killed rather than refused printed nothing at all. So it is not
left anonymous: it carries a marker naming what it was seeding, where it was
going, and which process was writing it.

```bash
cargo shared-target --prune ~/projects
```

```
staged tree  /home/you/projects/thing/workspace/target.partial
  from       /home/you/projects/thing/target
  for        /home/you/projects/thing/workspace/target
  written    by pid 48213 5 days ago, cargo-shared-target 0.1.0
  holds      124018 files  46.0 GiB
  shared     40.1 GiB of that is a second name for a file another tree also has
  frees      at most 5.9 GiB
  status     left alone; pass --remove to take it
```

Nothing is removed until `--remove` says so.

The marker is looked for rather than the name. `--staging` puts the tree
anywhere and `.partial` is a default rather than a promise, so a search by name
finds whichever trees happened to keep it. The search does not descend into a
directory Cargo has tagged as a target, which is what keeps it from listing
hundreds of thousands of entries — a staged tree put inside one is the case this
does not find.

**`du` is not the answer to how much room this gets back.** Where the filesystem
cannot clone blocks, the artifacts under `deps/` are one inode under two names:
`du` on the staged tree counts every byte of them, and removing it frees none,
because what goes is a name and not the storage. What is printed above is the
part that has no other name. Where the filesystem *can* clone blocks, every file
has a name of its own and shares its storage underneath, so that figure reads as
the whole tree and is an upper bound — the tree cost little to make and gives
little back.

A tree something is still writing is left alone. That is answered by taking the
marker's lock rather than by reading a pid out of it: a process that is killed
releases its locks, and one that recorded that it was running did not.

The last thing a finished run does is take its marker off, which happens after
the rename rather than before — a rename that failed would otherwise leave a
tree with nothing in it to say what it was. So a run killed in that one moment
leaves a marker sitting on a real target directory. That is not mistaken for an
abandoned tree, because the marker names the destination it is sitting in, and
only the marker is cleared.

## Layout

- `crates/shared-target-core` — the library. What Cargo knows, and typed errors.
- `crates/cargo-shared-target` — the subcommand.

## License

MIT or Apache-2.0, at your option.
