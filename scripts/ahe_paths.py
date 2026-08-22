#!/usr/bin/env python3
"""Shared path derivation for the AHE hooks (stdlib only, cross-platform).

Why this module exists
----------------------
guard_engine.py / reflect.py / log_metric.py / check_destructive_delete.py all
need the same two roots, and all five used to HARDCODE one literal project-slug
string -- the original author's checkout path, slugified. Anyone who clones the
repo somewhere else gets a directory that does not exist, and the guard engine
then fails open and dies silently (exactly the outage found on 2026-08-22, where
guards.jsonl had been gone for five days without a single visible symptom).

Two roots, deliberately different in KIND:

  repo root   The checkout this hook was invoked from. Derived from THIS FILE's
              location (<repo>/scripts/ahe_paths.py) -- never from cwd, never
              from an environment variable. That makes resolution identical in
              production and inside the sandboxed test harness, and it is exactly
              right for worktrees: .claude/settings.json runs
              "${CLAUDE_PROJECT_DIR}/scripts/guard_engine.py", so the worktree's
              own scripts read the worktree's own tracked registry.
              TRACKED project data lives here:  .claude/guards.jsonl

  state dir   ~/.claude/projects/<slug>/ for the MAIN checkout -- shared by the
              main repo and every worktree, so escalation state and hit history
              are global rather than per-branch.
              UNTRACKED runtime state lives here: guard_hits.jsonl /
              guard_state.json / metrics/ / ahe_backlog.md

The slug is Claude Code's own project-directory naming rule: take the absolute
path and replace every character outside [A-Za-z0-9-] with '-'. For example
"C:\\src\\my_app" becomes "C--src-my-app", and a worktree of it keeps the parent's
prefix. Deliberately NOT illustrated with this machine's real paths: the rule is
cross-checked against whatever ~/.claude/projects actually contains on the host
that runs scripts/test_guards.py, which is stronger evidence than a baked-in
example and does not leak a checkout location into a public repository.

Every function is total: callers are hooks that must never raise.
"""
import os
import re

# <repo>/scripts/ahe_paths.py -> <repo>
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# A Claude Code worktree lives at <main>/.claude/worktrees/<name>.
_WORKTREE_RE = re.compile(r"^(?P<main>.*?)[\\/]\.claude[\\/]worktrees[\\/][^\\/]+[\\/]?$")

# Everything Claude Code does NOT keep verbatim in a project directory name.
_SLUG_UNSAFE_RE = re.compile(r"[^A-Za-z0-9-]")


def main_checkout(path=None):
    """The main checkout for `path`; `path` itself when it is not a worktree."""
    p = os.path.normpath(os.path.abspath(path or REPO_ROOT))
    m = _WORKTREE_RE.match(p)
    return m.group("main") if m else p


def slug(path):
    """Claude Code's project-directory name for an absolute path."""
    p = os.path.normpath(os.path.abspath(path))
    # normpath keeps a drive-root trailing separator ("F:\\"); a trailing
    # separator would otherwise produce a spurious trailing '-'.
    p = p.rstrip("\\/") or p
    return _SLUG_UNSAFE_RE.sub("-", p)


def state_dir(path=None):
    """~/.claude/projects/<main-checkout-slug>/ -- shared by all worktrees."""
    return os.path.join(os.path.expanduser("~"), ".claude", "projects",
                        slug(main_checkout(path)))


def guards_file(path=None):
    """The TRACKED rule registry: <repo>/.claude/guards.jsonl."""
    return os.path.join(os.path.abspath(path or REPO_ROOT), ".claude", "guards.jsonl")


def guard_state_file(path=None):
    """The UNTRACKED escalation overlay (rule id -> action). git-external."""
    return os.path.join(state_dir(path), "guard_state.json")
