#!/usr/bin/env python3
"""List directories in the checkout that contain no files at all (r.md #81).

Why this exists
---------------
`NVIDIA Corporation/umdlogs/` kept appearing in the repository root and in
`daw_gui/` / `ui/crates/renderer/` -- the NVIDIA user-mode driver's log
directory, created relative to the process CWD because `%ProgramData%` was
unset in a `make` recipe. `daw_guitestsfixtures/` appeared the same way, from a
shell losing the backslashes in `daw_gui\\tests\\fixtures`.

**git cannot see either of them.** git does not track directories, so a
directory holding no files does not exist as far as git is concerned:

  - it is not `??` (untracked) in `git status`
  - it is not `!!` (ignored) either -- `git check-ignore` exits 1 for them,
    which only means "no ignore rule matched", NOT "the tree is clean"
  - adding them to `.gitignore` changes nothing, because they were never in
    the output to begin with. It would only promote the cause into a
    "known harmless" label while the cause keeps producing them.

So the only way to notice a recurrence is to walk the filesystem. That is what
this script does, and `scripts/arch_lint.sh` runs it as a standing gate.

What counts as a hit
--------------------
A directory whose subtree contains **zero files** -- reported at its topmost
such directory. That matters: `NVIDIA Corporation/` is not itself empty (it
holds `umdlogs/`), so a plain `-type d -empty` scan names the child and hides
the actual offender. Reporting the topmost all-empty directory names
`NVIDIA Corporation` instead.

No allowlist is needed. git cannot represent an empty directory, so every
directory that comes from a checkout necessarily contains at least one tracked
file; a placeholder (`.gitkeep`) would itself be a file and would take the
directory out of scope. Verified: this repository tracks no such placeholder.

Failure is never silence
------------------------
If the walk cannot run, or sees implausibly few directories, this exits
non-zero with a message instead of printing an empty (= "all clean") list.
`--self-test` proves the detector actually detects, on a synthetic tree, so a
clean report from a broken detector cannot pass for a clean repository.
"""
import os
import sys
import tempfile

# Build outputs and vendored trees: not ours, and huge. Pruned by NAME at any
# depth (a nested `target/` is just as uninteresting as the top-level one).
PRUNE_NAMES = frozenset((
    ".git", "target", "third_party", "node_modules", "__pycache__", "dist",
))

# Pruned by path relative to the scan root: other worktrees are separate
# checkouts with their own arch-lint run, so scanning them from here would
# report another branch's litter as this branch's problem.
PRUNE_RELPATHS = (os.path.join(".claude", "worktrees"),)

# A checkout of this repository has hundreds of directories. Seeing almost none
# means the walk did not actually run (wrong root, permissions, a broken
# prune list) -- the "no plugins found because we never looked" failure mode.
MIN_SCANNED_DIRS = 50


class ScanError(Exception):
    """The scan could not be trusted. Never degrade this into an empty list."""


def _repo_root():
    # <repo>/scripts/empty_dirs.py -> <repo>. Derived from this file, never
    # from cwd, so it is identical under make, under a hook and in a test.
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def scan(root):
    """Topmost directories whose subtree holds no files, plus the dir count.

    Returns (hits, scanned). `hits` are '/'-separated paths relative to
    `root`, sorted, so the output is stable across platforms and across the
    order the filesystem happens to hand back entries.
    """
    if not os.path.isdir(root):
        raise ScanError("scan root does not exist: %s" % root)

    # Walk top-down so pruning actually prunes: mutating `dirnames` stops
    # os.walk from descending. (`topdown=False` would let us fold bottom-up
    # for free, but it descends into `target/` first -- measured at ~50x the
    # cost on this checkout -- and its `dirnames` edits are ignored.)
    visited = []  # (dirpath, kept child names, has own files) in parent-first order
    for dirpath, dirnames, filenames in os.walk(root, onerror=_raise):
        rel = os.path.relpath(dirpath, root)
        base = "" if rel == os.curdir else rel
        dirnames[:] = [d for d in dirnames
                       if not _is_pruned(os.path.join(base, d) if base else d)]
        visited.append((dirpath, list(dirnames), bool(filenames)))
    scanned = len(visited)

    # Fold bottom-up: reversing a top-down walk puts every child before its
    # parent. True == "this subtree holds at least one file".
    has_file = {}
    for dirpath, kept, own_files in reversed(visited):
        has_file[dirpath] = own_files or any(
            has_file.get(os.path.join(dirpath, d), False) for d in kept
        )

    if scanned < MIN_SCANNED_DIRS:
        raise ScanError(
            "only %d directories scanned under %s (expected at least %d) -- "
            "the walk did not run, so 'no hits' would be a false green"
            % (scanned, root, MIN_SCANNED_DIRS)
        )

    hits = []
    for dirpath, empty_free in sorted(has_file.items()):
        if empty_free or dirpath == root:
            continue
        # topmost only: skip when the parent is empty too, so the report names
        # `NVIDIA Corporation` rather than `NVIDIA Corporation/umdlogs`.
        parent = os.path.dirname(dirpath)
        if parent in has_file and not has_file[parent]:
            continue
        hits.append(os.path.relpath(dirpath, root).replace(os.sep, "/"))
    return sorted(hits), scanned


def _raise(err):
    raise ScanError("cannot read %s: %s" % (getattr(err, "filename", "?"), err))


def _is_pruned(rel):
    """True for a pruned directory **or anything under one**.

    The self-test caught the "or anything under one" half: pruning only the
    exact path let `.claude/worktrees/wt/empty` through and reported another
    branch's litter as this branch's.
    """
    norm = rel.replace("\\", "/").strip("/")
    parts = norm.split("/")
    if any(p in PRUNE_NAMES for p in parts):
        return True
    for pruned in PRUNE_RELPATHS:
        p = pruned.replace(os.sep, "/")
        if norm == p or norm.startswith(p + "/"):
            return True
    return False


def self_test():
    """Prove the detector detects, on a tree we build on purpose.

    Covers the three ways it could silently stop working: missing a hit,
    inventing one, and naming the child instead of the offending parent.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # Enough real directories to clear MIN_SCANNED_DIRS, each with a file
        # so they are legitimately non-empty.
        for i in range(MIN_SCANNED_DIRS + 5):
            d = os.path.join(tmp, "pkg%02d" % i)
            os.makedirs(d)
            with open(os.path.join(d, "src.rs"), "w", encoding="utf-8") as fh:
                fh.write("// real\n")
        # The NVIDIA shape: a parent that is not itself empty, holding only an
        # empty child. The parent is the hit.
        os.makedirs(os.path.join(tmp, "NVIDIA Corporation", "umdlogs"))
        # The daw_guitestsfixtures shape: a plain empty directory.
        os.makedirs(os.path.join(tmp, "daw_guitestsfixtures"))
        # Must be pruned even though it is empty.
        os.makedirs(os.path.join(tmp, "target", "debug", "empty"))
        os.makedirs(os.path.join(tmp, ".claude", "worktrees", "wt", "empty"))
        with open(os.path.join(tmp, ".claude", "settings.json"), "w",
                  encoding="utf-8") as fh:
            fh.write("{}\n")

        hits, scanned = scan(tmp)
        want = ["NVIDIA Corporation", "daw_guitestsfixtures"]
        if hits != want:
            return "detector wrong: got %r want %r (scanned %d)" % (hits, want, scanned)

    # And it must refuse to report "clean" when it saw almost nothing.
    with tempfile.TemporaryDirectory() as tmp:
        os.makedirs(os.path.join(tmp, "only", "one"))
        try:
            scan(tmp)
        except ScanError:
            return None
        return "detector accepted an implausibly small walk (no false-green guard)"
    return None


def main(argv):
    if "--self-test" in argv:
        problem = self_test()
        if problem:
            sys.stderr.write("empty_dirs.py: SELF-TEST FAILED: %s\n" % problem)
            return 2
        print("empty-dirs self-test ok")
        return 0

    root = argv[1] if len(argv) > 1 else _repo_root()
    try:
        hits, _ = scan(root)
    except ScanError as exc:
        sys.stderr.write("empty_dirs.py: %s\n" % exc)
        return 2
    for hit in hits:
        print(hit)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
