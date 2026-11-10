# RustyPac

Repository: https://git.phrk.org/pub/RustyPac

RustyPac is a small external downloader for pacman on Arch Linux/CachyOS. It uses bytehaul for segmented HTTP downloads and resumable known-length transfers, writes to pacman's exact `.part` path, and shows a clean adaptive progress row. Pacman still chooses mirrors, applies signature policy, and performs the final `.part` rename.

**Download parallelism:** RustyPac can split one file across multiple connections. Pacman's `XferCommand` interface still supplies files one at a time; it does not support parallel package-file downloads. See [the parallel-download limitation](#parallel-package-files-pacman-limitation) before choosing RustyPac over pacman's built-in downloader.

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

`--enable` inserts the active line immediately before the commented curl/wget `XferCommand` examples in `[options]`, leaving those examples intact. If no examples exist, it inserts before the next repository section. It also relocates an active command left below the examples by an older release. `--enable` is a no-op when RustyPac is already active in the correct location. Otherwise it asks `Enable RustyPac? [y/N]`; only `Y` or `y` changes `/etc/pacman.conf`. When run without root privileges, it first says that changing `pacman.conf` requires sudo. An existing active `XferCommand` is retained as:

```text
## Pre-RustyPac XferCommand ## XferCommand = ...
```

The resulting placement in a standard configuration is:

```ini
[options]
# ... existing options ...
XferCommand = /usr/local/bin/RustyPac %u %o
#XferCommand = /usr/bin/curl -L -C - -f -o %o %u
#XferCommand = /usr/bin/wget --passive-ftp -c -O %o %u
# ... remaining options and repository definitions ...
```

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

## Download configuration

### File location and automatic recovery

On `--enable` or a download, RustyPac reads:

```text
$HOME/.config/RustyPac/config.yaml
```

- **Missing file:** creates the directory and writes the defaults below.
- **Valid file:** reads your settings without rewriting the file or removing comments. Omitted settings use defaults.
- **Corrupt or invalid file:** warns and replaces it with defaults. This includes malformed YAML, unknown setting names, wrong types, and out-of-range values.
- **I/O error:** reports the problem and stops, rather than overwriting an unreadable file.

Settings are read on each invocation; no rebuild or service restart is needed. `--disable` works independently of the download configuration.

### Supported settings and defaults

```yaml
resume: true                  # false = restart existing incomplete downloads
connections_per_file: 4       # simultaneous split connections, 1..128
connections_per_server: 4     # cap for this file's server, 1..128
min_split_size: 10485760      # split files at/above this many bytes (10 MiB)
piece_size: 1048576           # resume/split piece size in bytes (1 MiB)
connect_timeout_secs: 30
read_timeout_secs: 60
max_retries: 5                # retry attempts per segment; 0 = none
max_download_speed: 0         # bytes/sec per file; 0 = unlimited
```

Each download has one URL, so its effective connection limit is the **smaller** of `connections_per_file` and `connections_per_server`. Servers without range support still use one stream. `min_split_size` must be positive, `piece_size` is 1 byte through 64 MiB, and timeouts are 1 through 86400 seconds. With `resume: false`, incomplete transfers restart rather than using saved checkpoints, including after suspend/continue.

### Tuning connections and resume behavior

To allow up to eight connections for each file, edit **both** limits in your existing file:

```yaml
connections_per_file: 8
connections_per_server: 8
```

Increasing only one limit leaves the other as the cap. Files below `min_split_size`, or servers without range support, do not gain extra connections just because the limits are higher. Adjust `min_split_size` if you want smaller files to be eligible for splitting.

Keep `resume: true` to reuse valid incomplete-download checkpoints. Set `resume: false` to download incomplete files again from the beginning. Resume depends on a known-length transfer and valid bytehaul state; an arbitrary existing `.part` file alone does not guarantee resume.

### Selecting the configuration when using sudo

The path uses the **process's HOME**: `sudo` commonly changes it to `/root`, so normal-user and sudo invocations can use different files. `RUSTYPAC_CONFIG=/absolute/path/config.yaml` overrides the location. For example, to make a sudo pacman invocation use your existing user config explicitly:

```sh
sudo env RUSTYPAC_CONFIG="$HOME/.config/RustyPac/config.yaml" pacman -Syu
```

Create/edit the user file as your normal user first; if root creates it through this override, it will be root-owned. Paths must be accessible to the account running RustyPac.

### Parallel package files: pacman limitation

**RustyPac cannot make pacman download multiple package files in parallel through `XferCommand`.** Pacman passes one URL/output pair and waits for the external command before passing the next. Its `ParallelDownloads` option controls the built-in downloader, not this external callback. Consequently there is no misleading `parallel_files` setting here. The YAML connection settings speed up segments **within a file**, not concurrent package files.

For parallel package files today, disable RustyPac (`sudo /usr/local/bin/RustyPac --disable`) and use pacman's built-in `ParallelDownloads`. Supporting parallel package files with bytehaul requires a separate batch-aware integration; returning success before a `.part` file is complete would break pacman's contract. See pacman's [external callback loop](https://github.com/devkitPro/pacman/blob/master/lib/libalpm/dload.c) and [synchronous command execution](https://github.com/devkitPro/pacman/blob/master/src/pacman/conf.c).

## Progress and recovery

Routine active frames update at one-second intervals and re-probe terminal width; `SIGWINCH` forces an immediate redraw. TTY output skips unchanged frames and finishes with one newline. It selects `Full`, `Small`, `Minimal`, `Compact`, `Plain`, then `Extreme` as space decreases. Structured state rows keep fixed field boundaries, and interactive output reserves one right-edge cell to prevent autowrap. Structured rows reserve at least 23 display cells for the filename; Unicode-safe truncation uses `...`. Full shows a bar, percentage, transferred/total, speed, and `DONE`/`ETA`; smaller modes progressively omit fields. Active content is white, success green, unavailable optional signatures dark gray with a broken-chain icon, errors red, and structured separators purple. Redirected output is plain, flushed, newline-delimited, and rate-limited the same way.

With the default `resume: true`, known-length range transfers use bytehaul resume state at `<OUTPUT>.bytehaul`; a no-range server falls back to one stream. `SIGINT`, `SIGTERM`, and `SIGHUP` cancel cooperatively, retain valid partial/resume state, restore the terminal, and exit nonzero. `SIGTSTP` checkpoints before stopping; `SIGCONT` revalidates, resumes, and redraws. With `resume: false`, incomplete transfers restart instead of reusing saved checkpoints. `SIGKILL` cannot be handled, but the next invocation recovers a verified stale lock.

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
