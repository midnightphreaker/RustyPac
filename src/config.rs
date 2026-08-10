use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PRESERVED_PREFIX: &[u8] = b"## Pre-RustyPac XferCommand ## ";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ConfigState {
    Active,
    Disabled,
    Absent,
    Conflicting { line: String },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DisableChoice {
    Existing,
    Default,
}

#[derive(Debug)]
pub struct EditPlan {
    original: Vec<u8>,
    replacement: Vec<u8>,
    initial_state: ConfigState,
    expected_state: ConfigState,
    executable: String,
}

impl EditPlan {
    pub fn initial_state(&self) -> &ConfigState {
        &self.initial_state
    }

    pub fn changed(&self) -> bool {
        self.original != self.replacement
    }

    pub fn contents(&self) -> &[u8] {
        &self.replacement
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Invalid(String),
    ChangedDuringEdit,
    Io(std::io::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid pacman configuration: {message}"),
            Self::ChangedDuringEdit => {
                formatter.write_str("pacman configuration changed while the edit was pending")
            }
            Self::Io(error) => write!(formatter, "pacman configuration update failed: {error}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    content_end: usize,
    end: usize,
}

impl Line {
    fn content<'a>(&self, contents: &'a [u8]) -> &'a [u8] {
        &contents[self.start..self.content_end]
    }
}

#[derive(Debug)]
struct Analysis {
    state: ConfigState,
    rusty: Option<Line>,
    rusty_disabled: bool,
    conflicting: Option<Line>,
    preserved: Option<Line>,
}

pub fn plan_enable(contents: &[u8], executable: &str) -> Result<EditPlan, ConfigError> {
    let analysis = analyze(contents, executable)?;
    let initial_state = analysis.state.clone();
    if analysis.state == ConfigState::Active {
        return Ok(EditPlan {
            original: contents.to_vec(),
            replacement: contents.to_vec(),
            initial_state,
            expected_state: ConfigState::Active,
            executable: executable.to_owned(),
        });
    }

    if analysis.conflicting.is_some() && analysis.preserved.is_some() {
        return Err(ConfigError::Invalid(
            "both an active downloader and a preserved downloader are present".to_owned(),
        ));
    }

    let active = active_command(executable);
    let newline = preferred_newline(contents);
    let mut edits = Vec::new();
    if let Some(conflicting) = analysis.conflicting {
        let mut preserved = PRESERVED_PREFIX.to_vec();
        preserved.extend_from_slice(conflicting.content(contents));
        edits.push((conflicting.start, conflicting.content_end, preserved));
    }
    if let Some(rusty) = analysis.rusty {
        if analysis.rusty_disabled {
            let indentation = leading_indentation(rusty.content(contents));
            let mut enabled = indentation.to_vec();
            enabled.extend_from_slice(active.as_bytes());
            edits.push((rusty.start, rusty.content_end, enabled));
        }
    } else if let Some(conflicting) = analysis.conflicting {
        let mut inserted = Vec::new();
        if conflicting.end == conflicting.content_end {
            inserted.extend_from_slice(newline);
        }
        inserted.extend_from_slice(active.as_bytes());
        inserted.extend_from_slice(newline);
        edits.push((conflicting.end, conflicting.end, inserted));
    } else {
        let mut inserted = Vec::new();
        if !contents.is_empty() && !contents.ends_with(b"\n") {
            inserted.extend_from_slice(newline);
        }
        inserted.extend_from_slice(active.as_bytes());
        inserted.extend_from_slice(newline);
        edits.push((contents.len(), contents.len(), inserted));
    }

    Ok(EditPlan {
        original: contents.to_vec(),
        replacement: apply_edits(contents, edits),
        initial_state,
        expected_state: ConfigState::Active,
        executable: executable.to_owned(),
    })
}

pub fn plan_disable(contents: &[u8], choice: DisableChoice) -> Result<EditPlan, ConfigError> {
    let executable = "/usr/local/bin/RustyPac";
    let analysis = analyze(contents, executable)?;
    let initial_state = analysis.state.clone();
    if analysis.state != ConfigState::Active {
        return Ok(EditPlan {
            original: contents.to_vec(),
            replacement: contents.to_vec(),
            expected_state: initial_state.clone(),
            initial_state,
            executable: executable.to_owned(),
        });
    }

    let rusty = analysis.rusty.ok_or_else(|| {
        ConfigError::Invalid("active RustyPac command could not be located".to_owned())
    })?;
    let mut commented = leading_indentation(rusty.content(contents)).to_vec();
    commented.push(b'#');
    commented.extend_from_slice(active_command(executable).as_bytes());
    let mut edits = vec![(rusty.start, rusty.content_end, commented)];
    let expected_state = if choice == DisableChoice::Existing {
        if let Some(preserved) = analysis.preserved {
            let line = trim_ascii(preserved.content(contents));
            let restored = line[PRESERVED_PREFIX.len()..].to_vec();
            let restored_text = String::from_utf8(restored.clone()).map_err(|_| {
                ConfigError::Invalid("preserved XferCommand is not valid UTF-8".to_owned())
            })?;
            edits.push((preserved.start, preserved.content_end, restored));
            ConfigState::Conflicting {
                line: restored_text,
            }
        } else {
            ConfigState::Disabled
        }
    } else {
        ConfigState::Disabled
    };

    Ok(EditPlan {
        original: contents.to_vec(),
        replacement: apply_edits(contents, edits),
        initial_state,
        expected_state,
        executable: executable.to_owned(),
    })
}

pub fn apply(path: &Path, plan: EditPlan) -> Result<(), ConfigError> {
    if !plan.changed() {
        return Ok(());
    }

    let current = fs::read(path)?;
    if current != plan.original {
        return Err(ConfigError::ChangedDuringEdit);
    }
    let metadata = fs::metadata(path)?;
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Invalid("configuration path has no parent directory".to_owned())
    })?;
    let (temporary_path, mut temporary) = create_temporary(parent, path)?;

    let staged = (|| -> Result<(), ConfigError> {
        temporary.write_all(&plan.replacement)?;
        temporary.sync_all()?;
        preserve_metadata(&temporary, &metadata)?;
        temporary.sync_all()?;
        temporary.seek(SeekFrom::Start(0))?;
        let mut reread = Vec::new();
        temporary.read_to_end(&mut reread)?;
        if reread != plan.replacement {
            return Err(ConfigError::Invalid(
                "staged configuration did not match the planned bytes".to_owned(),
            ));
        }
        let staged_analysis = analyze(&reread, &plan.executable)?;
        if staged_analysis.state != plan.expected_state {
            return Err(ConfigError::Invalid(
                "staged configuration did not have the planned state".to_owned(),
            ));
        }
        fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if staged.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    staged
}

pub fn state(contents: &[u8]) -> Result<ConfigState, ConfigError> {
    Ok(analyze(contents, "/usr/local/bin/RustyPac")?.state)
}

pub fn has_preserved_downloader(contents: &[u8]) -> Result<bool, ConfigError> {
    Ok(analyze(contents, "/usr/local/bin/RustyPac")?
        .preserved
        .is_some())
}

fn analyze(contents: &[u8], executable: &str) -> Result<Analysis, ConfigError> {
    let active = active_command(executable);
    let mut rusty = None;
    let mut rusty_disabled = false;
    let mut conflicting = None;
    let mut preserved = None;

    for line in lines(contents) {
        let raw = line.content(contents);
        let trimmed = trim_ascii(raw);
        if trimmed.starts_with(PRESERVED_PREFIX) {
            if preserved.replace(line).is_some() {
                return Err(ConfigError::Invalid(
                    "multiple preserved XferCommand lines".to_owned(),
                ));
            }
            let original = &trimmed[PRESERVED_PREFIX.len()..];
            validate_xfer_command(original)?;
            continue;
        }

        if let Some(commented) = strip_comment(trimmed) {
            if commented == active.as_bytes() {
                if rusty.replace(line).is_some() {
                    return Err(ConfigError::Invalid(
                        "multiple RustyPac XferCommand lines".to_owned(),
                    ));
                }
                rusty_disabled = true;
            } else if commented.starts_with(b"XferCommand")
                && commented
                    .windows(executable.len())
                    .any(|part| part == executable.as_bytes())
            {
                return Err(ConfigError::Invalid(
                    "malformed commented RustyPac XferCommand".to_owned(),
                ));
            }
            continue;
        }

        if starts_with_xfer_command(trimmed) {
            validate_xfer_command(trimmed)?;
            if trimmed == active.as_bytes() {
                if rusty.replace(line).is_some() {
                    return Err(ConfigError::Invalid(
                        "multiple RustyPac XferCommand lines".to_owned(),
                    ));
                }
                rusty_disabled = false;
            } else if conflicting.replace(line).is_some() {
                return Err(ConfigError::Invalid(
                    "multiple active XferCommand lines".to_owned(),
                ));
            }
        }
    }

    if rusty.is_some() && !rusty_disabled && conflicting.is_some() {
        return Err(ConfigError::Invalid(
            "RustyPac and another XferCommand are both active".to_owned(),
        ));
    }
    let state = if rusty.is_some() && !rusty_disabled {
        ConfigState::Active
    } else if let Some(conflict) = conflicting {
        let line = String::from_utf8(conflict.content(contents).to_vec()).map_err(|_| {
            ConfigError::Invalid("active XferCommand is not valid UTF-8".to_owned())
        })?;
        ConfigState::Conflicting { line }
    } else if rusty.is_some() {
        ConfigState::Disabled
    } else {
        ConfigState::Absent
    };

    Ok(Analysis {
        state,
        rusty,
        rusty_disabled,
        conflicting,
        preserved,
    })
}

fn lines(contents: &[u8]) -> Vec<Line> {
    let mut result = Vec::new();
    let mut start = 0;
    for (index, byte) in contents.iter().enumerate() {
        if *byte == b'\n' {
            let content_end = if index > start && contents[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            result.push(Line {
                start,
                content_end,
                end: index + 1,
            });
            start = index + 1;
        }
    }
    if start < contents.len() {
        result.push(Line {
            start,
            content_end: contents.len(),
            end: contents.len(),
        });
    }
    result
}

fn active_command(executable: &str) -> String {
    format!("XferCommand = {executable} %u %o")
}

fn starts_with_xfer_command(line: &[u8]) -> bool {
    line.starts_with(b"XferCommand")
        && line
            .get(b"XferCommand".len())
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'=')
}

fn validate_xfer_command(line: &[u8]) -> Result<(), ConfigError> {
    if !starts_with_xfer_command(line) {
        return Err(ConfigError::Invalid(
            "malformed preserved XferCommand".to_owned(),
        ));
    }
    let remainder = trim_ascii_start(&line[b"XferCommand".len()..]);
    let Some(remainder) = remainder.strip_prefix(b"=") else {
        return Err(ConfigError::Invalid(
            "XferCommand is missing '='".to_owned(),
        ));
    };
    if trim_ascii(remainder).is_empty() {
        return Err(ConfigError::Invalid(
            "XferCommand has no command".to_owned(),
        ));
    }
    Ok(())
}

fn strip_comment(line: &[u8]) -> Option<&[u8]> {
    let remainder = line.strip_prefix(b"#")?;
    Some(trim_ascii_start(remainder))
}

fn leading_indentation(line: &[u8]) -> &[u8] {
    let length = line
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    &line[..length]
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let line = trim_ascii_start(line);
    let length = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &line[..length]
}

fn trim_ascii_start(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    &line[start..]
}

fn preferred_newline(contents: &[u8]) -> &'static [u8] {
    if contents.windows(2).any(|window| window == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    }
}

fn apply_edits(contents: &[u8], mut edits: Vec<(usize, usize, Vec<u8>)>) -> Vec<u8> {
    edits.sort_by_key(|edit| edit.0);
    let extra: usize = edits.iter().map(|edit| edit.2.len()).sum();
    let mut output = Vec::with_capacity(contents.len() + extra);
    let mut copied = 0;
    for (start, end, replacement) in edits {
        output.extend_from_slice(&contents[copied..start]);
        output.extend_from_slice(&replacement);
        copied = end;
    }
    output.extend_from_slice(&contents[copied..]);
    output
}

fn create_temporary(parent: &Path, target: &Path) -> Result<(PathBuf, File), ConfigError> {
    let name = target
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("configuration path has no filename".to_owned()))?
        .to_string_lossy();
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{name}.rustypac-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ConfigError::Io(error)),
        }
    }
    Err(ConfigError::Invalid(
        "could not allocate a temporary configuration file".to_owned(),
    ))
}

fn preserve_metadata(file: &File, metadata: &fs::Metadata) -> Result<(), ConfigError> {
    let result = unsafe {
        libc::fchown(
            file.as_raw_fd(),
            metadata.uid() as libc::uid_t,
            metadata.gid() as libc::gid_t,
        )
    };
    if result != 0 {
        return Err(ConfigError::Io(std::io::Error::last_os_error()));
    }
    let result =
        unsafe { libc::fchmod(file.as_raw_fd(), (metadata.mode() & 0o7777) as libc::mode_t) };
    if result != 0 {
        return Err(ConfigError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}
