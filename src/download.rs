use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytehaul::{
    DownloadError, DownloadSpec, DownloadState, Downloader, FileAllocation, ProgressSnapshot,
};
use tokio::sync::watch;

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
    Running,
    Interrupt,
}

#[derive(Clone)]
pub struct ControlHandle {
    sender: watch::Sender<ControlRequest>,
}

pub struct ControlEvents {
    receiver: watch::Receiver<ControlRequest>,
    #[cfg(test)]
    defer_interrupt_until_terminal: bool,
}

pub fn control_channel() -> (ControlHandle, ControlEvents) {
    let (sender, receiver) = watch::channel(ControlRequest::Running);
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
    let (sender, receiver) = watch::channel(ControlRequest::Running);
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
    let mut progress = handle.subscribe_progress();

    if renderer.update(&model, false).is_err() {
        handle.cancel();
        let _ = handle.wait().await;
        return DownloadOutcome::Failed;
    }

    let mut events_open = true;
    #[cfg(test)]
    let defer_interrupt_until_terminal = events.defer_interrupt_until_terminal;
    #[cfg(not(test))]
    let defer_interrupt_until_terminal = false;
    let result = if defer_interrupt_until_terminal {
        let mut terminal_observer = progress.clone();
        while !is_terminal(terminal_observer.borrow().state) {
            if terminal_observer.changed().await.is_err() {
                break;
            }
        }
        if events.receiver.changed().await.is_ok()
            && *events.receiver.borrow_and_update() == ControlRequest::Interrupt
        {
            handle.cancel();
        }
        handle.wait().await
    } else {
        loop {
            tokio::select! {
                changed = progress.changed() => {
                    if changed.is_err() {
                        break handle.wait().await;
                    }
                    let snapshot = progress.borrow_and_update().clone();
                    apply_snapshot(&mut model, &snapshot, started.elapsed());
                    if renderer.update(&model, false).is_err() {
                        handle.cancel();
                        break handle.wait().await;
                    }
                    if is_terminal(snapshot.state) {
                        break handle.wait().await;
                    }
                }
                changed = events.receiver.changed(), if events_open => {
                    match changed {
                        Ok(()) if *events.receiver.borrow_and_update() == ControlRequest::Interrupt => {
                            handle.cancel();
                            break handle.wait().await;
                        }
                        Ok(()) => {}
                        Err(_) => events_open = false,
                    }
                }
            }
        }
    };

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
