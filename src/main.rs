mod cli;
pub mod config;
pub mod download;
mod lock;
pub mod progress;
pub mod render;
pub mod signals;

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use cli::Command;
use download::DownloadOutcome;
use render::{Renderer, TerminalInfo};
use signals::SignalEvent;

fn main() -> ExitCode {
    match cli::parse_args(std::env::args()) {
        Ok(Command::Download { url, output }) => run_download(&url, &output),
        Ok(Command::Enable) => run_enable(&config_path()),
        Ok(Command::Disable) => run_disable(&config_path()),
        Err(error) => {
            eprintln!("RustyPac: {error}");
            ExitCode::from(2)
        }
    }
}

const PACMAN_CONF: &str = "/etc/pacman.conf";
const INSTALLED_EXECUTABLE: &str = "/usr/local/bin/RustyPac";

fn config_path() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("RUSTYPAC_PACMAN_CONF") {
        return PathBuf::from(path);
    }
    PathBuf::from(PACMAN_CONF)
}

fn run_enable(path: &Path) -> ExitCode {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) => return config_failure("read", error, path),
    };
    let plan = match config::plan_enable(&contents, INSTALLED_EXECUTABLE) {
        Ok(plan) => plan,
        Err(error) => return config_failure("inspect", error, path),
    };
    if plan.initial_state() == &config::ConfigState::Active {
        eprintln!("RustyPac is already active");
        return ExitCode::SUCCESS;
    }
    note_sudo_requirement();
    if !confirm("Enable RustyPac? [y/N]") {
        eprintln!("RustyPac enable cancelled");
        return ExitCode::SUCCESS;
    }
    match config::apply(path, plan) {
        Ok(()) => {
            eprintln!("RustyPac is now active");
            ExitCode::SUCCESS
        }
        Err(error) => config_failure("update", error, path),
    }
}

fn run_disable(path: &Path) -> ExitCode {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) => return config_failure("read", error, path),
    };
    let state = match config::state(&contents) {
        Ok(state) => state,
        Err(error) => return config_failure("inspect", error, path),
    };
    match state {
        config::ConfigState::Absent => {
            eprintln!("RustyPac is absent");
            return ExitCode::SUCCESS;
        }
        config::ConfigState::Disabled => {
            eprintln!("RustyPac is already disabled");
            return ExitCode::SUCCESS;
        }
        config::ConfigState::Conflicting { line } => {
            eprintln!("RustyPac is absent; another XferCommand is active: {line}");
            return ExitCode::SUCCESS;
        }
        config::ConfigState::Active => {}
    }
    note_sudo_requirement();
    if !confirm("Disable RustyPac? [y/N]") {
        eprintln!("RustyPac disable cancelled");
        return ExitCode::SUCCESS;
    }
    let choice = match config::has_preserved_downloader(&contents) {
        Ok(true) => {
            let answer = prompt(
                "Revert to existing XferCommand [e] or to default pacman behaviour? [enter]",
            );
            if matches!(answer.as_deref(), Ok("e" | "E")) {
                config::DisableChoice::Existing
            } else {
                config::DisableChoice::Default
            }
        }
        Ok(false) => config::DisableChoice::Default,
        Err(error) => return config_failure("inspect", error, path),
    };
    let plan = match config::plan_disable(&contents, choice) {
        Ok(plan) => plan,
        Err(error) => return config_failure("inspect", error, path),
    };
    match config::apply(path, plan) {
        Ok(()) => {
            eprintln!("RustyPac is now disabled");
            ExitCode::SUCCESS
        }
        Err(error) => config_failure("update", error, path),
    }
}

fn note_sudo_requirement() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("RustyPac: changing pacman.conf requires sudo");
    }
}

fn confirm(question: &str) -> bool {
    matches!(prompt(question).as_deref(), Ok("y" | "Y"))
}

fn prompt(question: &str) -> std::io::Result<String> {
    eprint!("{question} ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    while answer.ends_with(['\n', '\r']) {
        answer.pop();
    }
    Ok(answer)
}

fn config_failure(action: &str, error: impl std::fmt::Display, path: &Path) -> ExitCode {
    eprintln!("RustyPac: failed to {action} {}: {error}", path.display());
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("RustyPac: rerun with sudo to modify pacman.conf");
    }
    ExitCode::from(1)
}

fn run_download(url: &str, output: &std::path::Path) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("RustyPac: failed to start async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let started = Instant::now();
    let mut renderer = Renderer::new(
        std::io::stdout(),
        || {
            let stdout = std::io::stdout();
            let width = terminal_size::terminal_size()
                .map(|(terminal_size::Width(width), _)| usize::from(width))
                .unwrap_or(80);
            if stdout.is_terminal() {
                TerminalInfo::terminal(width)
            } else {
                TerminalInfo::redirected(width)
            }
        },
        move || started.elapsed(),
    );
    let outcome = runtime.block_on(async {
        let mut signal_events = match signals::subscribe() {
            Ok(events) => events,
            Err(error) => {
                eprintln!("RustyPac: failed to subscribe to Unix signals: {error}");
                return DownloadOutcome::Failed;
            }
        };
        let (control, events) = download::control_channel();
        let signal_task = tokio::spawn(async move {
            while let Some(event) = signal_events.recv().await {
                match event {
                    SignalEvent::Interrupt | SignalEvent::Terminate | SignalEvent::Hangup => {
                        control.interrupt();
                    }
                    SignalEvent::Suspend => control.suspend(),
                    SignalEvent::Continue => control.continue_transfer(),
                    SignalEvent::Resize => control.resize(),
                }
                signals::record_test_delivery(event);
            }
        });
        let outcome = download::run(url, output, events, &mut renderer).await;
        signal_task.abort();
        outcome
    });

    if outcome == DownloadOutcome::Completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
