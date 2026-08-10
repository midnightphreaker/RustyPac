# RustyPac design

## Purpose

RustyPac is a focused external downloader for pacman. It uses bytehaul for segmented HTTP transfers and resume, writes to pacman's exact `.part` path, renders a stable adaptive progress row, and can enable or disable itself without overwriting unrelated pacman configuration.

## Components

- `cli`: distinguishes `URL OUTPUT`, `--enable`, and `--disable`; validates arguments and maps outcomes to exit codes.
- `pacman_config`: parses only relevant `XferCommand` lines and applies atomic targeted transformations to `/etc/pacman.conf`.
- `download`: builds bytehaul specifications, owns the transfer lifecycle, validates output, and translates bytehaul states/errors.
- `transfer_lock`: serializes writers per output using recorded process identity and removes or recovers stale locks.
- `signals`: converts INT, TERM, HUP, TSTP, CONT, and terminal-resize events into lifecycle actions.
- `progress`: reduces bytehaul snapshots and lifecycle events into a renderer-independent display model.
- `render`: selects a width mode, formats display-cell-safe rows, applies terminal colors, and controls refresh timing.

Each component exposes typed data rather than terminal strings or bytehaul internals across boundaries. This keeps download correctness independently testable from presentation and pacman configuration edits.

## Download flow

For `RustyPac URL OUTPUT`, the CLI validates an absolute or relative output without changing its spelling, acquires an output-specific lock, and starts bytehaul with that exact path. Known-length range-capable transfers are segmented and resumable through `<OUTPUT>.bytehaul`; non-range servers fall back to one stream. Unknown-length transfers use one stream without a resume guarantee.

Progress snapshots update an internal display model continuously, but terminal rendering is rate-limited to one changed frame per second. A completion or failure event bypasses the timer. Success additionally requires the requested output to be readable and nonempty. RustyPac leaves renaming to pacman.

Pacman owns mirror iteration. Any failed RustyPac invocation returns nonzero so a later invocation may use another URL with the same partial target.

## Responsive rendering

The renderer measures terminal display cells, including emoji sequences, rather than UTF-8 bytes. It chooses the richest fitting mode:

1. `Full`: filename, 20-cell bar and percentage, transferred/total size, speed, `DONE`/`ETA` and time.
2. `Small`: filename, percentage, current size, speed, time.
3. `Minimal`: filename, percentage, speed, time.
4. `Compact`: the Minimal data without border glyphs.
5. `Plain`: unpadded fields separated by spaces; skipped rows contain one `N/A`.
6. `Extreme`: Plain ellipsized as a whole to terminal width.

In modes 1-4, non-filename columns have deterministic widths and the filename receives every surplus cell. A mode remains selected while the filename has at least 23 cells; otherwise the next mode is tried. Long filenames are end-truncated with three ASCII dots.

TTY frames use carriage return and erase-to-end-of-line, never an intermediate blank clear. Unchanged frames are skipped. `SIGWINCH` causes an immediate mode recalculation and redraw. A terminal state emits immediately and ends the row with exactly one newline. Redirected output contains no ANSI or cursor controls and emits flushed newline records no more than once per second plus the terminal record.

Content is white while active, green after success, dark gray for skipped/unavailable state, and red on error. Structured separators remain purple in all states.

## Errors and signatures

HTTP 404 or 410 is shown as a skip only when the requested filename ends exactly in `.db.sig`. The row uses the broken-chain emoji and unavailable fields, but RustyPac still returns nonzero so pacman's `DatabaseOptional` setting remains authoritative. Package signatures and every other failure remain red hard errors.

Internal bytehaul diagnostics are reduced to one user-facing error row. Detailed errors remain available to tests and exit-state logic without being dumped into pacman's normal progress output.

## Signals, locks, and recovery

INT, TERM, and HUP request cooperative bytehaul cancellation, allow a resume checkpoint, restore the terminal, remove transient lock state, and exit nonzero. TSTP checkpoints and pauses, restores the terminal, and then suspends the process; CONT revalidates state, resumes, and redraws immediately.

The per-output lock stores process identity robust enough to distinguish a live owner from PID reuse. Startup removes a verified-stale lock before validating bytehaul's sidecar. SIGKILL is uncatchable, so this next-start recovery is the cleanup guarantee. Valid partial output is retained after interruption; successful transfers remove lock and bytehaul control state.

## Enable and disable flow

`RustyPac --enable` reports success without editing if already active. Otherwise it tells non-root users that the operation requires sudo and changes nothing unless the user enters `Y` or `y`. A different active downloader is preserved in place as:

```text
## Pre-RustyPac XferCommand ## XferCommand = ...
```

RustyPac then inserts or uncomments its own `XferCommand` line. The write uses a same-directory temporary file, preserves unrelated content and metadata, and atomically replaces the original only after validation.

`RustyPac --disable` reports whether RustyPac is already disabled or absent. When active it requires `Y` or `y` confirmation. If a preserved downloader exists, it asks:

```text
Revert to existing XferCommand [e] or to default pacman behaviour? [enter]
```

`E` or `e` restores the preserved command. Every other response keeps it preserved and comments out RustyPac, causing pacman to use its built-in downloader. With no preserved command, confirmed disablement only comments out RustyPac. No whole-file backup is restored.

## Verification strategy

Focused unit tests cover display width, all responsive thresholds, colors, formatters, configuration transformations, and lock identity. PTY tests cover redraw timing, resize, control sequences, and final newlines. Local HTTP fixtures cover ranges, no ranges, unknown length, status classification, interruption/resume, cross-URL continuation, and byte integrity. Process tests cover catchable signals and stale-state recovery. An integration test runs as `alpm` with an absolute `.part` target.

After independent review passes, the live final gate enables RustyPac and runs `sudo pacman -Syy vim --noconfirm`. Failure disables RustyPac to restore pacman's built-in downloader. The old aria2 integration is neither enabled nor used as fallback.

## Scope limits

RustyPac is one pacman-oriented binary. It does not implement aria2 metadata compatibility, a daemon, a GUI, mirror selection, package management, unknown-length resume, or live connection counting.
