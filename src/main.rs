mod cli;
pub mod download;
mod lock;
pub mod progress;
pub mod render;
pub mod signals;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Instant;

use cli::Command;
use download::DownloadOutcome;
use render::{Renderer, TerminalInfo};
use signals::SignalEvent;

fn main() -> ExitCode {
    match cli::parse_args(std::env::args()) {
        Ok(Command::Download { url, output }) => run_download(&url, &output),
        Ok(Command::Enable | Command::Disable) => {
            eprintln!("RustyPac: command is not yet wired");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("RustyPac: {error}");
            ExitCode::from(2)
        }
    }
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
