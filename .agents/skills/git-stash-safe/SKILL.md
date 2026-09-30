---
name: git-stash-safe
description: Safely use git stash in xlite without stripping feature dependencies or breaking the build. Use when stashing unrelated changes, cleaning the worktree before check/test, or recovering from a stash that may have removed needed files.
---

# Safe `git stash` in xlite

## Symptom / misjudgment
Agent stashes "fmt noise" or "unrelated edits", then reports tests green — but `Cargo.toml` / core modules were stashed away and the tree no longer compiles. Phase 4 reviews caught this repeatedly.

## Hard constraints (also in AGENTS.md §二)
1. Message must honestly describe contents (`git stash push -m "..."`).
2. Before stash: `git diff --stat` and confirm no current-feature dependency is removed.
3. After stash: `cargo check --tests` is mandatory.
4. Never stash `Cargo.toml` / `Cargo.lock` / build scripts — `git checkout` them or commit separately.
5. **`git stash pop` / `apply` restores the *index*, not just the worktree** — including untracked paths captured by `-u`. A stray stage entry then rides into the next `git commit` (which commits the whole index), i.e. a rule-1「夹带」. Real incident: a `stash -u` swept up an untracked `admin/node_modules` symlink, the pop re-staged it, and the next commit carried it into PR #50 (removed in #58).

## Procedure
1. `git status` and `git diff --stat` — list every path that would leave the worktree.
2. If any path is required by the current feature, do **not** stash it; commit, leave unstaged, or split the change.
3. `git stash push -m "<honest summary of what is being hidden>"`.
4. Immediately run `cargo check --tests`.
5. If check fails: `git stash pop` (or apply), restore the missing dependency, re-evaluate. Do not claim verification while the tree is broken.
6. After **any** pop/apply: `git diff --cached --stat` — every path listed there must be one you intend to commit. `git restore --staged <path>` the rest **before** staging your own files.
7. Only after a green check may you proceed with the intended commit/PR work.

## Verification
- `cargo check --tests` exit 0 on the post-stash worktree.
- Stash message would let a human restore the right content without guessing.
- `git diff --cached --stat` right before every commit lists only the intended paths (mandatory after a pop/apply).
