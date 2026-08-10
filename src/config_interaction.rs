use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::config::{self, ConfigState, DisableChoice};

const PACMAN_CONF: &str = "/etc/pacman.conf";
const INSTALLED_EXECUTABLE: &str = "/usr/local/bin/RustyPac";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RunStatus {
    Success,
    Failure,
}

pub fn production_config_path() -> &'static Path {
    Path::new(PACMAN_CONF)
}

pub fn run_enable<R: BufRead, W: Write>(path: &Path, input: &mut R, output: &mut W) -> RunStatus {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) => return config_failure(output, "read", error, path),
    };
    let plan = match config::plan_enable(&contents, INSTALLED_EXECUTABLE) {
        Ok(plan) => plan,
        Err(error) => return config_failure(output, "inspect", error, path),
    };
    if plan.initial_state() == &ConfigState::Active {
        let _ = writeln!(output, "RustyPac is already active");
        return RunStatus::Success;
    }
    note_sudo_requirement(output);
    match confirm(input, output, "Enable RustyPac? [y/N]") {
        Ok(true) => {}
        Ok(false) => {
            let _ = writeln!(output, "RustyPac enable cancelled");
            return RunStatus::Success;
        }
        Err(error) => return config_failure(output, "read response for", error, path),
    }
    match config::apply(path, plan) {
        Ok(outcome) => {
            report_cleanup_warning(output, &outcome);
            let _ = writeln!(output, "RustyPac is now active");
            RunStatus::Success
        }
        Err(error) => config_failure(output, "update", error, path),
    }
}

pub fn run_disable<R: BufRead, W: Write>(path: &Path, input: &mut R, output: &mut W) -> RunStatus {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) => return config_failure(output, "read", error, path),
    };
    let state = match config::state(&contents) {
        Ok(state) => state,
        Err(error) => return config_failure(output, "inspect", error, path),
    };
    match state {
        ConfigState::Absent => {
            let _ = writeln!(output, "RustyPac is absent");
            return RunStatus::Success;
        }
        ConfigState::Disabled => {
            let _ = writeln!(output, "RustyPac is already disabled");
            return RunStatus::Success;
        }
        ConfigState::Conflicting { line } => {
            let _ = writeln!(
                output,
                "RustyPac is absent; another XferCommand is active: {line}"
            );
            return RunStatus::Success;
        }
        ConfigState::Active => {}
    }
    note_sudo_requirement(output);
    match confirm(input, output, "Disable RustyPac? [y/N]") {
        Ok(true) => {}
        Ok(false) => {
            let _ = writeln!(output, "RustyPac disable cancelled");
            return RunStatus::Success;
        }
        Err(error) => return config_failure(output, "read response for", error, path),
    }
    let choice = match config::has_preserved_downloader(&contents) {
        Ok(true) => match prompt(
            input,
            output,
            "Revert to existing XferCommand [e] or to default pacman behaviour? [enter]",
        ) {
            Ok(answer) if matches!(answer.as_str(), "e" | "E") => DisableChoice::Existing,
            Ok(_) => DisableChoice::Default,
            Err(error) => return config_failure(output, "read response for", error, path),
        },
        Ok(false) => DisableChoice::Default,
        Err(error) => return config_failure(output, "inspect", error, path),
    };
    let plan = match config::plan_disable(&contents, choice) {
        Ok(plan) => plan,
        Err(error) => return config_failure(output, "inspect", error, path),
    };
    match config::apply(path, plan) {
        Ok(outcome) => {
            report_cleanup_warning(output, &outcome);
            let _ = writeln!(output, "RustyPac is now disabled");
            RunStatus::Success
        }
        Err(error) => config_failure(output, "update", error, path),
    }
}

fn report_cleanup_warning(output: &mut impl Write, outcome: &config::ApplyOutcome) {
    if let config::ApplyOutcome::AppliedWithCleanupPending { staging_directory } = outcome {
        let _ = writeln!(
            output,
            "RustyPac: configuration was updated, but cleanup remains at {}",
            staging_directory.display()
        );
    }
}

fn note_sudo_requirement(output: &mut impl Write) {
    if unsafe { libc::geteuid() } != 0 {
        let _ = writeln!(output, "RustyPac: changing pacman.conf requires sudo");
    }
}

fn confirm(input: &mut impl BufRead, output: &mut impl Write, question: &str) -> io::Result<bool> {
    Ok(matches!(
        prompt(input, output, question)?.as_str(),
        "y" | "Y"
    ))
}

fn prompt(input: &mut impl BufRead, output: &mut impl Write, question: &str) -> io::Result<String> {
    write!(output, "{question} ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    while answer.ends_with(['\n', '\r']) {
        answer.pop();
    }
    Ok(answer)
}

fn config_failure(
    output: &mut impl Write,
    action: &str,
    error: impl std::fmt::Display,
    path: &Path,
) -> RunStatus {
    let _ = writeln!(
        output,
        "RustyPac: failed to {action} {}: {error}",
        path.display()
    );
    if unsafe { libc::geteuid() } != 0 {
        let _ = writeln!(output, "RustyPac: rerun with sudo to modify pacman.conf");
    }
    RunStatus::Failure
}
