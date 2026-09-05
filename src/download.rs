use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytehaul::{
    DownloadError, DownloadHandle, DownloadSpec, DownloadState, Downloader, FileAllocation,
    ProgressSnapshot,
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::lock::OutputLock;
use crate::progress::{DisplayState, ProgressModel};
use crate::render::{Clock, Renderer, TerminalProbe};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadOutcome {
    Completed,
    SkippedDatabaseSignature,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlRequest {
    Interrupt,
    Suspend,
    Continue,
    Resize,
}

#[derive(Clone)]
pub struct ControlHandle {
    sender: mpsc::UnboundedSender<ControlRequest>,
}

pub struct ControlEvents {
    receiver: mpsc::UnboundedReceiver<ControlRequest>,
    #[cfg(test)]
    defer_interrupt_until_terminal: bool,
}

pub fn control_channel() -> (ControlHandle, ControlEvents) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        ControlHandle { sender },
        ControlEvents {
            receiver,
            #[cfg(test)]
            defer_interrupt_until_terminal: false,
        },
    )
}

#[cfg(test)]
pub fn terminal_boundary_control_channel() -> (ControlHandle, ControlEvents) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        ControlHandle { sender },
        ControlEvents {
            receiver,
            defer_interrupt_until_terminal: true,
        },
    )
}

impl ControlHandle {
    pub fn interrupt(&self) {
        let _ = self.sender.send(ControlRequest::Interrupt);
    }

    pub fn suspend(&self) {
        let _ = self.sender.send(ControlRequest::Suspend);
    }

    pub fn continue_transfer(&self) {
        let _ = self.sender.send(ControlRequest::Continue);
    }

    pub fn resize(&self) {
        let _ = self.sender.send(ControlRequest::Resize);
    }
}

enum TransferCycle {
    Finished(Result<(), DownloadError>),
    Suspended(PauseTransition),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PauseTransition {
    Stop,
    Resume,
    Interrupt,
}

struct ResumeCheckpoint {
    path: PathBuf,
    bytes: Vec<u8>,
    output_path: PathBuf,
    output_fingerprint: OutputFingerprint,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    content_hash: [u8; 32],
}

impl OutputFingerprint {
    fn capture(path: &Path) -> Option<Self> {
        let path_metadata = fs::symlink_metadata(path).ok()?;
        if !path_metadata.file_type().is_file() {
            return None;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        let mut fingerprint = Self::from_metadata(&file.metadata().ok()?);
        if fingerprint != Self::from_metadata(&path_metadata) {
            return None;
        }

        // Same-length writes can share both timestamps on coarse filesystem clocks.
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).ok()?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        if fingerprint != Self::from_metadata(&file.metadata().ok()?)
            || fingerprint != Self::from_metadata(&fs::symlink_metadata(path).ok()?)
        {
            return None;
        }
        fingerprint.content_hash = hasher.finalize().into();
        Some(fingerprint)
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            content_hash: [0; 32],
        }
    }

    fn still_matches(self, path: &Path) -> bool {
        Self::capture(path) == Some(self)
    }
}

impl ResumeCheckpoint {
    fn capture(output: &Path) -> Option<Self> {
        let path = sidecar_path(output);
        // Eligibility only: authoritative checkpoint bytes are read after the output.
        if !fs::symlink_metadata(&path).ok()?.file_type().is_file() {
            return None;
        }
        let output_fingerprint = OutputFingerprint::capture(output)?;
        let bytes = read_checkpoint(&path)?;
        if OutputFingerprint::capture(output)? != output_fingerprint {
            return None;
        }
        Some(Self {
            path,
            bytes,
            output_path: output.to_owned(),
            output_fingerprint,
            armed: true,
        })
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ResumeCheckpoint {
    fn drop(&mut self) {
        if self.armed
            && matches!(
                fs::symlink_metadata(&self.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            )
            && self.output_fingerprint.still_matches(&self.output_path)
        {
            let _ = restore_checkpoint_atomically(
                &self.path,
                &self.bytes,
                &self.output_path,
                self.output_fingerprint,
            );
        }
    }
}

pub async fn run<W, P, C>(
    url: &str,
    output: &Path,
    mut events: ControlEvents,
    renderer: &mut Renderer<W, P, C>,
) -> DownloadOutcome
where
    W: Write,
    P: TerminalProbe,
    C: Clock,
{
    let filename = display_filename(output);
    let started = Instant::now();
    let mut model = initial_model(filename);

    let _output_lock = match OutputLock::acquire(output) {
        Ok(lock) => lock,
        Err(_) => {
            return finish(
                renderer,
                &mut model,
                DisplayState::Error,
                DownloadOutcome::Failed,
            )
        }
    };
    let mut resume_checkpoint = ResumeCheckpoint::capture(output);

    #[cfg(test)]
    let defer_interrupt_until_terminal = events.defer_interrupt_until_terminal;
    #[cfg(not(test))]
    let defer_interrupt_until_terminal = false;
    let mut first_cycle = true;
    let result = loop {
        let downloader = match Downloader::builder().build() {
            Ok(downloader) => downloader,
            Err(_) => {
                return finish(
                    renderer,
                    &mut model,
                    DisplayState::Error,
                    DownloadOutcome::Failed,
                )
            }
        };
        let spec = DownloadSpec::new(url)
            .output_path(output.to_owned())
            .file_allocation(FileAllocation::None);
        let handle = downloader.download(spec);
        let progress = handle.subscribe_progress();

        model.state = DisplayState::Active;
        if renderer.update(&model, !first_cycle).is_err() {
            handle.cancel();
            let _ = handle.wait().await;
            return DownloadOutcome::Failed;
        }
        first_cycle = false;

        let cycle = if defer_interrupt_until_terminal {
            drive_terminal_boundary(handle, progress, &mut events).await
        } else {
            drive_transfer(handle, progress, &mut events, renderer, &mut model, started).await
        };

        match cycle {
            TransferCycle::Finished(result) => break result,
            TransferCycle::Suspended(PauseTransition::Interrupt) => {
                break Err(DownloadError::Cancelled);
            }
            TransferCycle::Suspended(PauseTransition::Resume) => {}
            TransferCycle::Suspended(PauseTransition::Stop) => {
                model.state = DisplayState::Paused;
                if renderer.suspend(&model).is_err() {
                    return finish(
                        renderer,
                        &mut model,
                        DisplayState::Error,
                        DownloadOutcome::Failed,
                    );
                }

                match reconcile_paused_controls(&mut events, renderer, &model).await {
                    Ok(PauseTransition::Interrupt) => break Err(DownloadError::Cancelled),
                    Ok(PauseTransition::Resume) => continue,
                    Ok(PauseTransition::Stop) => {}
                    Err(error) => break Err(error),
                }

                // The queue was yielded to and drained after paused-row finalization.
                // POSIX offers no atomic "drain controls and stop" operation, so a new
                // control delivered after the final empty read can still race this last
                // instruction. SIGSTOP itself is valid, uncatchable, and returns only
                // after another process delivers SIGCONT.
                if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
                    return finish(
                        renderer,
                        &mut model,
                        DisplayState::Error,
                        DownloadOutcome::Failed,
                    );
                }
            }
        }
    };

    if result.is_ok() {
        if let Some(checkpoint) = &mut resume_checkpoint {
            checkpoint.disarm();
        }
    }

    match result {
        Ok(()) => match validate_and_clean(output) {
            Ok(size) => {
                apply_success(&mut model, size, started.elapsed());
                finish(
                    renderer,
                    &mut model,
                    DisplayState::Success,
                    DownloadOutcome::Completed,
                )
            }
            Err(()) => finish(
                renderer,
                &mut model,
                DisplayState::Error,
                DownloadOutcome::Failed,
            ),
        },
        Err(DownloadError::HttpStatus {
            status: 404 | 410, ..
        }) if is_database_signature(url, output) => finish(
            renderer,
            &mut model,
            DisplayState::Skipped,
            DownloadOutcome::SkippedDatabaseSignature,
        ),
        Err(DownloadError::Cancelled) => finish(
            renderer,
            &mut model,
            DisplayState::Interrupted,
            DownloadOutcome::Interrupted,
        ),
        Err(_) => finish(
            renderer,
            &mut model,
            DisplayState::Error,
            DownloadOutcome::Failed,
        ),
    }
}

async fn drive_terminal_boundary(
    handle: DownloadHandle,
    mut progress: tokio::sync::watch::Receiver<ProgressSnapshot>,
    events: &mut ControlEvents,
) -> TransferCycle {
    while !is_terminal(progress.borrow().state) {
        if progress.changed().await.is_err() {
            break;
        }
    }
    if events.receiver.recv().await == Some(ControlRequest::Interrupt) {
        handle.cancel();
    }
    TransferCycle::Finished(handle.wait().await)
}

async fn drive_transfer<W, P, C>(
    handle: DownloadHandle,
    mut progress: tokio::sync::watch::Receiver<ProgressSnapshot>,
    events: &mut ControlEvents,
    renderer: &mut Renderer<W, P, C>,
    model: &mut ProgressModel,
    started: Instant,
) -> TransferCycle
where
    W: Write,
    P: TerminalProbe,
    C: Clock,
{
    let mut events_open = true;
    loop {
        tokio::select! {
            changed = progress.changed() => {
                if changed.is_err() {
                    return TransferCycle::Finished(handle.wait().await);
                }
                let snapshot = progress.borrow_and_update().clone();
                apply_snapshot(model, &snapshot, started.elapsed());
                if renderer.update(model, false).is_err() {
                    handle.cancel();
                    return TransferCycle::Finished(handle.wait().await);
                }
                if is_terminal(snapshot.state) {
                    return TransferCycle::Finished(handle.wait().await);
                }
            }
            request = events.receiver.recv(), if events_open => {
                let Some(request) = request else {
                    events_open = false;
                    continue;
                };

                let snapshot = progress.borrow().clone();
                apply_snapshot(model, &snapshot, started.elapsed());
                if is_terminal(snapshot.state) {
                    return TransferCycle::Finished(handle.wait().await);
                }

                match request {
                    ControlRequest::Interrupt => {
                        handle.cancel();
                        return TransferCycle::Finished(handle.wait().await);
                    }
                    ControlRequest::Suspend => {
                        handle.pause();
                        pause_checkpoint_test_gate().await;
                        let result = handle.wait().await;
                        let snapshot = progress.borrow().clone();
                        apply_snapshot(model, &snapshot, started.elapsed());
                        return match result {
                            Err(DownloadError::Paused) => {
                                model.state = DisplayState::Paused;
                                match reconcile_paused_controls(events, renderer, model).await {
                                    Ok(transition) => TransferCycle::Suspended(transition),
                                    Err(error) => TransferCycle::Finished(Err(error)),
                                }
                            }
                            terminal_result => TransferCycle::Finished(terminal_result),
                        };
                    }
                    ControlRequest::Continue | ControlRequest::Resize => {
                        if renderer.update(model, true).is_err() {
                            handle.cancel();
                            return TransferCycle::Finished(handle.wait().await);
                        }
                    }
                }
            }
        }
    }
}

async fn reconcile_paused_controls<W, P, C>(
    events: &mut ControlEvents,
    renderer: &mut Renderer<W, P, C>,
    model: &ProgressModel,
) -> Result<PauseTransition, DownloadError>
where
    W: Write,
    P: TerminalProbe,
    C: Clock,
{
    tokio::task::yield_now().await;
    let mut transition = PauseTransition::Stop;
    while let Ok(request) = events.receiver.try_recv() {
        match request {
            ControlRequest::Interrupt => transition = PauseTransition::Interrupt,
            ControlRequest::Suspend if transition != PauseTransition::Interrupt => {
                transition = PauseTransition::Stop;
            }
            ControlRequest::Continue if transition != PauseTransition::Interrupt => {
                transition = PauseTransition::Resume;
            }
            ControlRequest::Resize if transition != PauseTransition::Interrupt => {
                renderer.update(model, true)?;
            }
            ControlRequest::Suspend | ControlRequest::Continue | ControlRequest::Resize => {}
        }
    }
    Ok(transition)
}

#[cfg(debug_assertions)]
async fn pause_checkpoint_test_gate() {
    let Some(base) = std::env::var_os("RUSTYPAC_TEST_PAUSE_GATE") else {
        return;
    };
    let mut ready = base.clone();
    ready.push(".ready");
    let ready = PathBuf::from(ready);
    if fs::write(&ready, b"ready").is_err() {
        return;
    }
    let mut release = base;
    release.push(".release");
    let release = PathBuf::from(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release.exists() && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
}

#[cfg(not(debug_assertions))]
async fn pause_checkpoint_test_gate() {}

fn initial_model(filename: String) -> ProgressModel {
    ProgressModel {
        filename,
        state: DisplayState::Active,
        downloaded: 0,
        total: None,
        bytes_per_second: None,
        elapsed: Duration::ZERO,
        eta: None,
    }
}

fn apply_snapshot(model: &mut ProgressModel, snapshot: &ProgressSnapshot, elapsed: Duration) {
    model.downloaded = snapshot.downloaded;
    model.total = snapshot.total_size;
    model.bytes_per_second = positive_rounded(snapshot.speed_bytes_per_sec);
    model.elapsed = elapsed;
    model.eta = snapshot
        .eta_secs
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(Duration::from_secs_f64);
}

fn apply_success(model: &mut ProgressModel, size: u64, elapsed: Duration) {
    model.downloaded = size;
    model.total = Some(size);
    model.bytes_per_second = average_speed(size, elapsed).or(model.bytes_per_second);
    model.elapsed = elapsed;
    model.eta = None;
}

fn positive_rounded(value: f64) -> Option<u64> {
    (value.is_finite() && value > 0.0).then(|| value.round() as u64)
}

fn average_speed(size: u64, elapsed: Duration) -> Option<u64> {
    let seconds = elapsed.as_secs_f64();
    (seconds > 0.0).then(|| (size as f64 / seconds).round().max(1.0) as u64)
}

fn is_terminal(state: DownloadState) -> bool {
    !matches!(state, DownloadState::Pending | DownloadState::Downloading)
}

fn finish<W, P, C>(
    renderer: &mut Renderer<W, P, C>,
    model: &mut ProgressModel,
    state: DisplayState,
    outcome: DownloadOutcome,
) -> DownloadOutcome
where
    W: Write,
    P: TerminalProbe,
    C: Clock,
{
    model.state = state;
    if renderer.finish(model).is_ok() {
        outcome
    } else {
        DownloadOutcome::Failed
    }
}

fn validate_and_clean(output: &Path) -> Result<u64, ()> {
    let mut file = File::open(output).map_err(|_| ())?;
    let size = file.metadata().map_err(|_| ())?.len();
    if size == 0 {
        return Err(());
    }
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).map_err(|_| ())?;

    let sidecar = sidecar_path(output);
    match fs::remove_file(&sidecar) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    Ok(size)
}

fn sidecar_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".bytehaul");
    PathBuf::from(path)
}

fn read_checkpoint(path: &Path) -> Option<Vec<u8>> {
    const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CHECKPOINT_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).ok()?;
    let path_metadata = fs::symlink_metadata(path).ok()?;
    (metadata.dev() == path_metadata.dev()
        && metadata.ino() == path_metadata.ino()
        && metadata.len() == path_metadata.len())
    .then_some(bytes)
}

fn restore_checkpoint_atomically(
    path: &Path,
    bytes: &[u8],
    output: &Path,
    output_fingerprint: OutputFingerprint,
) -> std::io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().unwrap_or(path.as_os_str());

    for _ in 0..16 {
        let mut temp_name = filename.to_os_string();
        temp_name.push(format!(
            ".rustypac-restore-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let temp_path = parent.join(temp_name);
        let mut temp = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temp_path)
        {
            Ok(temp) => temp,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let write_result = temp.write_all(bytes).and_then(|()| temp.sync_all());
        drop(temp);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        if !output_fingerprint.still_matches(output) {
            let _ = fs::remove_file(&temp_path);
            return Ok(());
        }

        let publish_result = fs::hard_link(&temp_path, path);
        let _ = fs::remove_file(&temp_path);
        return publish_result;
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate checkpoint restore path",
    ))
}

fn display_filename(output: &Path) -> String {
    let filename = output
        .file_name()
        .unwrap_or(output.as_os_str())
        .to_string_lossy();
    filename
        .strip_suffix(".part")
        .unwrap_or(&filename)
        .to_owned()
}

fn is_database_signature(url: &str, output: &Path) -> bool {
    let url_path = url
        .split('#')
        .next()
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url);
    let url_filename = url_path.rsplit('/').next().unwrap_or_default();
    let output_filename = output
        .file_name()
        .unwrap_or(output.as_os_str())
        .to_string_lossy();
    url_filename.ends_with(".db.sig") && output_filename.ends_with(".db.sig.part")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_attempt_that_changes_output_does_not_restore_stale_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("changed.part");
        let sidecar = sidecar_path(&output);
        fs::write(&output, b"original partial bytes").unwrap();
        fs::write(&sidecar, b"old checkpoint").unwrap();
        let checkpoint = ResumeCheckpoint::capture(&output).unwrap();

        fs::remove_file(&sidecar).unwrap();
        fs::write(&output, b"changed! partial bytes").unwrap();
        drop(checkpoint);

        assert!(
            !sidecar.exists(),
            "checkpoint for superseded output bytes must not be restored"
        );
    }

    #[test]
    fn same_metadata_does_not_hide_changed_output_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("same-metadata.part");
        let sidecar = sidecar_path(&output);
        fs::write(&output, b"original partial bytes").unwrap();
        fs::write(&sidecar, b"old checkpoint").unwrap();
        let mut checkpoint = ResumeCheckpoint::capture(&output).unwrap();

        fs::remove_file(&sidecar).unwrap();
        fs::write(&output, b"changed! partial bytes").unwrap();
        let current = OutputFingerprint::capture(&output).unwrap();
        // Reproduce a filesystem clock tick shared by both same-length writes.
        checkpoint.output_fingerprint.modified_seconds = current.modified_seconds;
        checkpoint.output_fingerprint.modified_nanoseconds = current.modified_nanoseconds;
        checkpoint.output_fingerprint.changed_seconds = current.changed_seconds;
        checkpoint.output_fingerprint.changed_nanoseconds = current.changed_nanoseconds;
        drop(checkpoint);

        assert!(
            !sidecar.exists(),
            "equal metadata must not permit restoring a checkpoint for different bytes"
        );
    }

    #[test]
    fn failed_attempt_keeps_newer_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("current.part");
        let sidecar = sidecar_path(&output);
        fs::write(&output, b"original partial bytes").unwrap();
        fs::write(&sidecar, b"old checkpoint").unwrap();
        let checkpoint = ResumeCheckpoint::capture(&output).unwrap();

        fs::write(&output, b"current transfer bytes").unwrap();
        fs::write(&sidecar, b"current checkpoint").unwrap();
        drop(checkpoint);

        assert_eq!(fs::read(&sidecar).unwrap(), b"current checkpoint");
    }
}
