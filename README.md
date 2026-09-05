# RustyPac

Repository: https://git.phrk.org/pub/RustyPac

RustyPac is a small external downloader for pacman on Arch Linux/CachyOS. It uses bytehaul for segmented HTTP downloads and resumable known-length transfers, writes to pacman's exact `.part` path, and shows a clean adaptive progress row. Pacman still chooses mirrors, applies signature policy, and performs the final `.part` rename.

## Requirements

- Arch Linux or CachyOS on Linux, with pacman and its `alpm` download user.
- RustyPac is tested with Rust 1.97.1; `Cargo.toml` does not declare that as a minimum supported Rust version (the crate uses edition 2021).
- bytehaul `0.2.0` (pinned by `Cargo.toml`). bytehaul is deliberately a small, low-maturity dependency here; the included local HTTP and live-pacman gates cover the supported behavior.
- `sudo` for installation, configuration changes, and the optional privileged compatibility gate.

## Build and install

```sh
cargo build --release
./scripts/install.sh
```

The installer builds the release binary and installs `/usr/local/bin/RustyPac` as `root:root`, mode `0755`. It does not edit pacman's configuration.

## Enable or disable pacman integration

Enable the installed binary:

```sh
sudo /usr/local/bin/RustyPac --enable
```

`--enable` is a no-op when RustyPac is already active. Otherwise it asks `Enable RustyPac? [y/N]`; only `Y` or `y` changes `/etc/pacman.conf`. When run without root privileges, it first says that changing `pacman.conf` requires sudo. An existing active `XferCommand` is retained as:

```text
## Pre-RustyPac XferCommand ## XferCommand = ...
```

and RustyPac activates `XferCommand = /usr/local/bin/RustyPac %u %o`.

Disable RustyPac with:

```sh
sudo /usr/local/bin/RustyPac --disable
```

This reports a no-op when RustyPac is absent or already disabled. When active, it asks `Disable RustyPac? [y/N]`; only `Y` or `y` proceeds. If a previous downloader was preserved, it then asks:

```text
Revert to existing XferCommand [e] or to default pacman behaviour? [enter]
```

Enter `e` or `E` to restore that previous command. Press Enter (or give any other response) to comment out only RustyPac and use pacman's built-in downloader; the preserved line remains available. Use this default choice as the safe fallback if a live pacman transfer fails. RustyPac does not use aria2 as a runtime fallback.

Check the effective setting with `pacman-conf XferCommand`.

## Use

Pacman invokes RustyPac as:

```sh
/usr/local/bin/RustyPac URL OUTPUT
```

For direct local use after a release build:

```sh
target/release/RustyPac 'https://example.invalid/package.pkg.tar.zst' /tmp/package.pkg.tar.zst.part
```

The command accepts exactly `URL OUTPUT`. Keep `OUTPUT` as the desired pacman `.part` path: RustyPac never renames it. A zero exit means the output is complete, readable, and nonempty; failures and skipped optional database signatures return nonzero so pacman can follow its mirror/signature policy.

## Progress and recovery

Routine active frames update at one-second intervals and re-probe terminal width; `SIGWINCH` forces an immediate redraw. TTY output skips unchanged frames and finishes with one newline. It selects `Full`, `Small`, `Minimal`, `Compact`, `Plain`, then `Extreme` as space decreases. Structured state rows keep fixed field boundaries, and interactive output reserves one right-edge cell to prevent autowrap. Structured rows reserve at least 23 display cells for the filename; Unicode-safe truncation uses `...`. Full shows a bar, percentage, transferred/total, speed, and `DONE`/`ETA`; smaller modes progressively omit fields. Active content is white, success green, unavailable optional signatures dark gray with a broken-chain icon, errors red, and structured separators purple. Redirected output is plain, flushed, newline-delimited, and rate-limited the same way.

Known-length range transfers use bytehaul resume state at `<OUTPUT>.bytehaul`; a no-range server falls back to one stream. `SIGINT`, `SIGTERM`, and `SIGHUP` cancel cooperatively, retain valid partial/resume state, restore the terminal, and exit nonzero. `SIGTSTP` checkpoints before stopping; `SIGCONT` revalidates, resumes, and redraws. `SIGKILL` cannot be handled, but the next invocation recovers a verified stale lock.

An HTTP 404 or 410 is shown as an unavailable skip only for a matching `.db.sig` URL and `.db.sig.part` output. This supports pacman's `DatabaseOptional` policy; package signatures and all other errors are hard failures.

## Test

Run the normal checks from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The explicit `alpm` compatibility gate is ignored by default. It requires an `alpm` user, passwordless `sudo` for the test's ownership changes, and the release binary built above:

```sh
cargo test --test alpm_gate -- --ignored
```

The verified live gate was:

```sh
sudo pacman -Syy vim --noconfirm
```

## Limitations and troubleshooting

- Unknown-length responses are downloaded in one stream and do not promise resume.
- There is no aria2 control-file compatibility or fallback, daemon, GUI, mirror manager, package-manager features, or live connection count.
- If pacman cannot execute RustyPac, confirm `/usr/local/bin/RustyPac` exists and remains readable/executable by `alpm`, then check `pacman-conf XferCommand`.
- If a live transfer misbehaves, run `sudo /usr/local/bin/RustyPac --disable` and confirm with `y`. The existing-XferCommand choice appears only if a previous command was preserved; otherwise disabling directly restores pacman's built-in downloader.
