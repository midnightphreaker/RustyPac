# RustyPac README Canonicalization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `README.md` the canonical user-facing guide for current verified RustyPac behavior.

**Architecture:** This is one documentation-only task. Update four inaccurate or incomplete passages, verify each claim directly against source and tests, then run the repository's documented checks.

**Tech Stack:** Markdown, Rust/Cargo verification commands, Git.

## Global Constraints

- Do not change Rust source, tests, configuration, or installed behavior.
- Preserve accurate existing installation, CLI, downloader, recovery, testing, and limitation guidance.
- Describe only behavior verified by current source and tests.

---

### Task 1: Canonicalize README behavior

**Files:**
- Modify: `README.md:5-10`
- Modify: `README.md:69-75`
- Modify: `README.md:100-105`
- Update: `/home/mp/.codex/process/RustyPac/2026-08-11/IMPLEMENTATION_SESSION_05.md`

**Interfaces:**
- Consumes: current behavior in `Cargo.toml`, `src/render.rs`, `src/signals.rs`, `src/config_interaction.rs`, and their tests.
- Produces: an accurate canonical user guide; no program interface changes.

- [ ] **Step 1: Record the current documentation gaps**

Create `IMPLEMENTATION_SESSION_05.md` listing the four approved corrections: tested Rust version wording, fixed state boundaries, interactive right-edge margin/resize behavior, and conditional disable prompt.

- [ ] **Step 2: Update the README**

Change `README.md` so it states:

```text
- RustyPac is tested with Rust 1.97.1; Cargo.toml does not declare that as a minimum supported Rust version.
- Routine active frames update at one-second intervals and re-probe terminal width; SIGWINCH forces an immediate redraw.
- Structured state rows keep fixed field boundaries, and interactive output reserves one right-edge cell to prevent autowrap.
- The existing-XferCommand choice appears only if a previous command was preserved; otherwise disabling directly restores pacman's built-in downloader.
```

- [ ] **Step 3: Verify documentation claims**

Run:

```sh
rg -n 'rust-version|edition' Cargo.toml
rg -n 'usable_width|DRAW_INTERVAL|Resize|reserved_width' src/render.rs src/signals.rs src/download.rs
rg -n 'previous downloader|second prompt|right-edge|SIGWINCH|one-second|1.97.1' README.md
```

Expected: no `rust-version` declaration; source evidence for the renderer claims; all approved clarifications present in README.

- [ ] **Step 4: Run repository verification**

Run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
git diff --check
```

Expected: formatting and Clippy pass, 93 total tests: 91 pass and 2 privileged tests are ignored; release build and diff check pass.

- [ ] **Step 5: Review and commit**

Require a fresh documentation review with zero BLOCKING findings, then run:

```sh
git add README.md
git commit -m "docs: canonicalize current RustyPac behavior"
```

Commit on the `readme-canonicalization` branch. The controller will integrate or cherry-pick reviewed commits onto `main` and push `main`; do not push from the isolated worktree.
