# RustyPac README canonicalization design

## Goal

Make `README.md` the canonical user-facing guide for the current verified RustyPac behavior without changing the program.

## Changes

1. Describe routine one-second terminal-width reprobes, immediate `SIGWINCH` redraws, fixed field boundaries across states, and the one-cell interactive autowrap margin.
2. Clarify that the existing-`XferCommand` choice appears only when RustyPac previously preserved one.
3. Describe Rust 1.97.1 as the tested toolchain rather than an enforced minimum because `Cargo.toml` has no `rust-version` declaration.
4. Correct troubleshooting so it does not promise a second disable prompt when no prior downloader exists.

## Constraints

- Documentation-only change.
- Preserve accurate existing installation, CLI, downloader, recovery, testing, and limitation guidance.
- Verify all changed claims against current source and tests.

## Acceptance

- README contains no known contradiction with the current implementation.
- Documented commands remain valid.
- Repository formatting, tests, and release build still pass.
