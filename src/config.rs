use std::error::Error;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use std::sync::{Arc, Mutex};

const PRESERVED_PREFIX: &[u8] = b"## Pre-RustyPac XferCommand ## ";
const EDIT_LOCK_CONTENTS: &[u8] = b"RustyPac edit lock v1\n";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
type ApplyHook = Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(test)]
static APPLY_PRE_COMMIT_HOOK: Mutex<Option<ApplyHook>> = Mutex::new(None);

#[cfg(test)]
static APPLY_POST_VALIDATION_HOOK: Mutex<Option<ApplyHook>> = Mutex::new(None);

#[cfg(test)]
static APPLY_CLEANUP_BOUNDARY_HOOK: Mutex<Option<ApplyHook>> = Mutex::new(None);

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ApplyOutcome {
    Unchanged,
    Applied,
    AppliedWithCleanupPending { staging_directory: PathBuf },
}

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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CleanupOutcome {
    Complete,
    Pending,
}

struct StagingDirectory {
    path: PathBuf,
    directory: File,
    identity: FileIdentity,
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
        let insertion_point = options_insertion_point(contents)?;
        let mut inserted = Vec::new();
        if insertion_point > 0 && contents[insertion_point - 1] != b'\n' {
            inserted.extend_from_slice(newline);
        }
        inserted.extend_from_slice(active.as_bytes());
        inserted.extend_from_slice(newline);
        edits.push((insertion_point, insertion_point, inserted));
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
            let restored = preserved_payload(preserved.content(contents))
                .ok_or_else(|| {
                    ConfigError::Invalid("preserved XferCommand marker is malformed".to_owned())
                })?
                .to_vec();
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

pub fn apply(path: &Path, plan: EditPlan) -> Result<ApplyOutcome, ConfigError> {
    if !plan.changed() {
        return Ok(ApplyOutcome::Unchanged);
    }

    let _edit_lock = open_edit_lock(path)?;
    let (mut source, metadata, source_identity, current) = open_verified_source(path)?;
    if current != plan.original {
        return Err(ConfigError::ChangedDuringEdit);
    }
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Invalid("configuration path has no parent directory".to_owned())
    })?;
    let staging = create_staging_directory(parent, path)?;
    let (temporary_path, mut temporary) = create_temporary(&staging)?;
    let temporary_identity = verified_file_identity(&temporary, &temporary_path)?;

    let staged = (|| -> Result<ApplyOutcome, ConfigError> {
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
        staging.directory.sync_all()?;
        run_apply_pre_commit_hook(path);
        exchange_paths(path, &temporary_path)?;
        if let Err(error) = validate_exchange(
            path,
            &temporary_path,
            &mut source,
            source_identity,
            &mut temporary,
            temporary_identity,
            &plan,
        ) {
            rollback_exchange(path, &temporary_path, temporary_identity)?;
            return Err(error);
        }
        if File::open(parent)
            .and_then(|directory| directory.sync_all())
            .is_err()
        {
            return Ok(ApplyOutcome::AppliedWithCleanupPending {
                staging_directory: staging.path.clone(),
            });
        }
        run_apply_post_validation_hook(&temporary_path);
        let cleanup = cleanup_displaced(&staging, &temporary_path, &source, source_identity);
        if cleanup == CleanupOutcome::Complete {
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
            Ok(ApplyOutcome::Applied)
        } else {
            Ok(ApplyOutcome::AppliedWithCleanupPending {
                staging_directory: staging.path.clone(),
            })
        }
    })();

    if staged.is_err() {
        cleanup_if_identity(&temporary_path, temporary_identity);
        cleanup_staging_directory(&staging);
    }
    staged
}

fn open_edit_lock(path: &Path) -> Result<File, ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Invalid("configuration path has no parent directory".to_owned())
    })?;
    let lock_path = edit_lock_path(path)?;
    let mut created = false;
    let mut lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
    {
        Ok(file) => {
            created = true;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(ConfigError::Io)?,
        Err(error) => return Err(ConfigError::Io(error)),
    };
    lock_exclusive(&lock)?;
    let identity = verified_file_identity(&lock, &lock_path)?;
    if verified_path_identity(&lock_path)? != identity {
        return Err(ConfigError::ChangedDuringEdit);
    }
    let metadata = lock.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(ConfigError::Invalid(
            "edit lock is not owned by the current user".to_owned(),
        ));
    }
    if created {
        lock.write_all(EDIT_LOCK_CONTENTS)?;
        lock.sync_all()?;
        File::open(parent)?.sync_all()?;
    } else if read_descriptor(&mut lock)? != EDIT_LOCK_CONTENTS {
        return Err(ConfigError::Invalid(
            "edit lock path is occupied by another file".to_owned(),
        ));
    }
    let result = unsafe { libc::fchmod(lock.as_raw_fd(), 0o600) };
    if result != 0 {
        return Err(ConfigError::Io(std::io::Error::last_os_error()));
    }
    let metadata = lock.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o600 {
        return Err(ConfigError::Invalid(
            "edit lock permissions are unsafe".to_owned(),
        ));
    }
    if verified_path_identity(&lock_path)? != identity {
        return Err(ConfigError::ChangedDuringEdit);
    }
    Ok(lock)
}

fn edit_lock_path(path: &Path) -> Result<PathBuf, ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Invalid("configuration path has no parent directory".to_owned())
    })?;
    let filename = path
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("configuration path has no filename".to_owned()))?;
    let mut name = OsString::from(".");
    name.push(filename);
    name.push(".rustypac-edit.lock");
    Ok(parent.join(name))
}

fn open_verified_source(
    path: &Path,
) -> Result<(File, fs::Metadata, FileIdentity, Vec<u8>), ConfigError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                ConfigError::Invalid("configuration path is a symbolic link".to_owned())
            } else {
                ConfigError::Io(error)
            }
        })?;
    let metadata = file.metadata()?;
    let identity = verified_metadata_identity(&metadata)?;
    if verified_path_identity(path)? != identity {
        return Err(ConfigError::ChangedDuringEdit);
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok((file, metadata, identity, contents))
}

fn validate_exchange(
    path: &Path,
    temporary_path: &Path,
    source: &mut File,
    source_identity: FileIdentity,
    temporary: &mut File,
    temporary_identity: FileIdentity,
    plan: &EditPlan,
) -> Result<(), ConfigError> {
    if verified_path_identity(path)? != temporary_identity
        || verified_path_identity(temporary_path)? != source_identity
        || verified_file_identity(source, temporary_path)? != source_identity
        || verified_file_identity(temporary, path)? != temporary_identity
    {
        return Err(ConfigError::ChangedDuringEdit);
    }
    if read_descriptor(source)? != plan.original || read_descriptor(temporary)? != plan.replacement
    {
        return Err(ConfigError::ChangedDuringEdit);
    }
    Ok(())
}

fn rollback_exchange(
    path: &Path,
    temporary_path: &Path,
    temporary_identity: FileIdentity,
) -> Result<(), ConfigError> {
    if path_identity(path).ok() == Some(temporary_identity)
        && fs::symlink_metadata(temporary_path).is_ok()
    {
        exchange_paths(path, temporary_path)?;
        cleanup_if_identity(temporary_path, temporary_identity);
        return Ok(());
    }

    Err(ConfigError::ChangedDuringEdit)
}

fn cleanup_displaced(
    staging: &StagingDirectory,
    displaced_path: &Path,
    displaced: &File,
    displaced_identity: FileIdentity,
) -> CleanupOutcome {
    if verified_staging_directory(staging).is_err() {
        return CleanupOutcome::Pending;
    }
    run_apply_cleanup_boundary_hook(displaced_path);
    match path_identity(displaced_path) {
        Err(ConfigError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            cleanup_staging_directory(staging);
            return if staging.path.exists() {
                CleanupOutcome::Pending
            } else {
                CleanupOutcome::Complete
            };
        }
        Ok(identity) if identity == displaced_identity => {}
        _ => return CleanupOutcome::Pending,
    }
    if verified_file_identity(displaced, displaced_path).ok() != Some(displaced_identity) {
        return CleanupOutcome::Pending;
    }
    let prepared = CString::new("prepared").expect("fixed staging filename has no NUL byte");
    let result = unsafe { libc::unlinkat(staging.directory.as_raw_fd(), prepared.as_ptr(), 0) };
    if result != 0 {
        return CleanupOutcome::Pending;
    }
    cleanup_staging_directory(staging);
    if staging.path.exists() {
        CleanupOutcome::Pending
    } else {
        CleanupOutcome::Complete
    }
}

fn lock_exclusive(file: &File) -> Result<(), ConfigError> {
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(ConfigError::Io(error));
        }
    }
}

fn cleanup_if_identity(path: &Path, identity: FileIdentity) {
    if path_identity(path).ok() == Some(identity) {
        let _ = fs::remove_file(path);
    }
}

fn read_descriptor(file: &mut File) -> Result<Vec<u8>, ConfigError> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

fn verified_path_identity(path: &Path) -> Result<FileIdentity, ConfigError> {
    let metadata = fs::symlink_metadata(path)?;
    verified_metadata_identity(&metadata)
}

fn verified_file_identity(file: &File, _path: &Path) -> Result<FileIdentity, ConfigError> {
    verified_metadata_identity(&file.metadata()?)
}

fn verified_metadata_identity(metadata: &fs::Metadata) -> Result<FileIdentity, ConfigError> {
    if !metadata.is_file() {
        return Err(ConfigError::Invalid(
            "configuration path is not a regular file".to_owned(),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(ConfigError::Invalid(
            "configuration file has multiple hard links".to_owned(),
        ));
    }
    Ok(FileIdentity::from(metadata))
}

fn path_identity(path: &Path) -> Result<FileIdentity, ConfigError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(ConfigError::ChangedDuringEdit);
    }
    Ok(FileIdentity::from(&metadata))
}

fn exchange_paths(first: &Path, second: &Path) -> Result<(), ConfigError> {
    let first = CString::new(first.as_os_str().as_bytes())
        .map_err(|_| ConfigError::Invalid("configuration path contains a NUL byte".to_owned()))?;
    let second = CString::new(second.as_os_str().as_bytes()).map_err(|_| {
        ConfigError::Invalid("temporary configuration path contains a NUL byte".to_owned())
    })?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ConfigError::Io(std::io::Error::last_os_error()))
    }
}

impl From<&fs::Metadata> for FileIdentity {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_apply_pre_commit_hook(hook: Option<ApplyHook>) {
    *APPLY_PRE_COMMIT_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_apply_post_validation_hook(hook: Option<ApplyHook>) {
    *APPLY_POST_VALIDATION_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_apply_cleanup_boundary_hook(hook: Option<ApplyHook>) {
    *APPLY_CLEANUP_BOUNDARY_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

#[cfg(test)]
fn run_apply_pre_commit_hook(path: &Path) {
    let hook = APPLY_PRE_COMMIT_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(test)]
fn run_apply_post_validation_hook(path: &Path) {
    let hook = APPLY_POST_VALIDATION_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
fn run_apply_post_validation_hook(_path: &Path) {}

#[cfg(test)]
fn run_apply_cleanup_boundary_hook(path: &Path) {
    let hook = APPLY_CLEANUP_BOUNDARY_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
fn run_apply_cleanup_boundary_hook(_path: &Path) {}

#[cfg(not(test))]
fn run_apply_pre_commit_hook(_path: &Path) {}

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
        if let Some(original) = preserved_payload(raw) {
            if preserved.replace(line).is_some() {
                return Err(ConfigError::Invalid(
                    "multiple preserved XferCommand lines".to_owned(),
                ));
            }
            validate_xfer_command(trim_ascii(original))?;
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

fn preserved_payload(line: &[u8]) -> Option<&[u8]> {
    trim_ascii_start(line).strip_prefix(PRESERVED_PREFIX)
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

fn options_insertion_point(contents: &[u8]) -> Result<usize, ConfigError> {
    let config_lines = lines(contents);
    let options: Vec<usize> = config_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (trim_ascii(line.content(contents)) == b"[options]").then_some(index)
        })
        .collect();
    let [options_index] = options.as_slice() else {
        return Err(ConfigError::Invalid(
            "exactly one [options] section is required".to_owned(),
        ));
    };
    Ok(config_lines
        .iter()
        .skip(options_index + 1)
        .find(|line| is_section_header(trim_ascii(line.content(contents))))
        .map_or(contents.len(), |line| line.start))
}

fn is_section_header(line: &[u8]) -> bool {
    line.len() >= 2 && line.first() == Some(&b'[') && line.last() == Some(&b']')
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

fn create_staging_directory(parent: &Path, target: &Path) -> Result<StagingDirectory, ConfigError> {
    let name = target
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("configuration path has no filename".to_owned()))?
        .to_string_lossy();
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{name}.rustypac-stage-{}-{sequence}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {
                let directory = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                    .open(&path)?;
                let result = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
                if result != 0 {
                    let error = ConfigError::Io(std::io::Error::last_os_error());
                    let _ = fs::remove_dir(&path);
                    return Err(error);
                }
                let metadata = directory.metadata()?;
                if !metadata.is_dir()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o7777 != 0o700
                {
                    let _ = fs::remove_dir(&path);
                    return Err(ConfigError::Invalid(
                        "private staging directory has unsafe ownership or permissions".to_owned(),
                    ));
                }
                let identity = FileIdentity::from(&metadata);
                if directory_path_identity(&path)? != identity {
                    let _ = fs::remove_dir(&path);
                    return Err(ConfigError::ChangedDuringEdit);
                }
                return Ok(StagingDirectory {
                    path,
                    directory,
                    identity,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ConfigError::Io(error)),
        }
    }
    Err(ConfigError::Invalid(
        "could not allocate a private staging directory".to_owned(),
    ))
}

fn create_temporary(staging: &StagingDirectory) -> Result<(PathBuf, File), ConfigError> {
    verified_staging_directory(staging)?;
    let path = staging.path.join("prepared");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    Ok((path, file))
}

fn verified_staging_directory(staging: &StagingDirectory) -> Result<(), ConfigError> {
    let metadata = staging.directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
        || FileIdentity::from(&metadata) != staging.identity
        || directory_path_identity(&staging.path)? != staging.identity
    {
        return Err(ConfigError::ChangedDuringEdit);
    }
    Ok(())
}

fn directory_path_identity(path: &Path) -> Result<FileIdentity, ConfigError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(ConfigError::ChangedDuringEdit);
    }
    Ok(FileIdentity::from(&metadata))
}

fn cleanup_staging_directory(staging: &StagingDirectory) {
    if verified_staging_directory(staging).is_ok() {
        let _ = fs::remove_dir(&staging.path);
    }
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
