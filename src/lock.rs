use std::error::Error;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use std::sync::{Arc, Mutex};

#[cfg(test)]
type StaleRecoveryHook = Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(test)]
static STALE_RECOVERY_HOOK: Mutex<Option<StaleRecoveryHook>> = Mutex::new(None);

#[cfg(test)]
static TAKEOVER_WRITE_FAILURE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
static TAKEOVER_TRANSITION_HOOK: Mutex<Option<StaleRecoveryHook>> = Mutex::new(None);

#[cfg(test)]
static POST_TRANSITION_FAILURE: Mutex<Option<PathBuf>> = Mutex::new(None);

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct OutputLock {
    path: PathBuf,
    owner: OwnerRecord,
    identity: FileIdentity,
    file: File,
}

#[derive(Debug)]
pub enum LockError {
    Held {
        pid: u32,
    },
    Unverifiable {
        path: PathBuf,
        reason: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OwnerRecord {
    pid: u32,
    start_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct PreparedLock {
    path: PathBuf,
    owner: OwnerRecord,
    identity: FileIdentity,
    file: File,
}

impl OutputLock {
    pub fn acquire(output: &Path) -> Result<Self, LockError> {
        let path = lock_path(output);
        let owner = current_owner()?;

        match create_lock(&path, owner) {
            Ok(lock) => Ok(lock),
            Err(CreateError::Exists) => acquire_existing_lock(path, owner),
            Err(CreateError::Io(error)) => Err(error),
        }
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        let Ok(owner) = read_owner_from_file(&mut self.file, &self.path) else {
            return;
        };
        if owner != self.owner {
            return;
        }

        let Ok(metadata) = fs::metadata(&self.path) else {
            return;
        };
        if FileIdentity::from(&metadata) == self.identity {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { pid } => write!(formatter, "output is locked by live process {pid}"),
            Self::Unverifiable { path, reason } => {
                write!(formatter, "cannot verify lock {}: {reason}", path.display())
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} lock {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for LockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Held { .. } | Self::Unverifiable { .. } => None,
        }
    }
}

enum CreateError {
    Exists,
    Io(LockError),
}

fn create_lock(path: &Path, owner: OwnerRecord) -> Result<OutputLock, CreateError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CreateError::Exists);
        }
        Err(source) => {
            return Err(CreateError::Io(io_error("create", path, source)));
        }
    };
    let identity = FileIdentity::from(
        &file
            .metadata()
            .map_err(|source| CreateError::Io(io_error("inspect", path, source)))?,
    );
    lock_exclusive(&file).map_err(|source| CreateError::Io(io_error("lock", path, source)))?;
    if let Err(source) = write_owner(&mut file, owner) {
        remove_created_if_unchanged(path, identity);
        return Err(CreateError::Io(io_error("write", path, source)));
    }

    Ok(OutputLock {
        path: path.to_owned(),
        owner,
        identity,
        file,
    })
}

fn acquire_existing_lock(path: PathBuf, owner: OwnerRecord) -> Result<OutputLock, LockError> {
    let path_identity = verified_path_identity(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|source| open_existing_error(&path, source))?;
    let identity = verified_file_identity(&file, &path)?;
    if identity != path_identity {
        return Err(lock_changed_error(&path));
    }

    let observed_owner = read_owner_from_file(&mut file, &path)?;
    match process_start_time(observed_owner.pid, &path)? {
        Some(start_time) if start_time == observed_owner.start_time => {
            return Err(LockError::Held {
                pid: observed_owner.pid,
            });
        }
        Some(_) | None => run_stale_recovery_hook(&path),
    }

    if !try_lock_exclusive(&file).map_err(|source| io_error("lock", &path, source))? {
        let pid = read_owner_from_file(&mut file, &path)?.pid;
        return Err(LockError::Held { pid });
    }

    ensure_path_identity(&path, identity)?;
    let locked_owner = read_owner_from_file(&mut file, &path)?;
    match process_start_time(locked_owner.pid, &path)? {
        Some(start_time) if start_time == locked_owner.start_time => {
            return Err(LockError::Held {
                pid: locked_owner.pid,
            });
        }
        Some(_) | None => {}
    }

    ensure_path_identity(&path, identity)?;
    let prepared = prepare_takeover(&path, owner)?;
    run_takeover_transition_hook(&path);

    if let Err(source) = exchange_paths(&path, &prepared.path) {
        cleanup_prepared(&prepared.path, prepared.identity);
        return Err(io_error("exchange", &path, source));
    }

    if let Err(error) = validate_transition(&path, &prepared, &file, identity)
        .and_then(|()| injected_post_transition_error(&path))
        .and_then(|()| remove_displaced_stale(&prepared.path, identity))
    {
        return Err(rollback_transition(
            &path,
            &prepared.path,
            prepared.identity,
            error,
        ));
    }

    Ok(OutputLock {
        path,
        owner: prepared.owner,
        identity: prepared.identity,
        file: prepared.file,
    })
}

fn prepare_takeover(path: &Path, owner: OwnerRecord) -> Result<PreparedLock, LockError> {
    let temp_path = next_temp_path(path);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|source| io_error("create takeover temporary", &temp_path, source))?;
    let identity = FileIdentity::from(
        &file
            .metadata()
            .map_err(|source| io_error("inspect", &temp_path, source))?,
    );
    if let Err(error) = verified_file_identity(&file, &temp_path) {
        cleanup_prepared(&temp_path, identity);
        return Err(error);
    }
    if let Err(source) = lock_exclusive(&file) {
        cleanup_prepared(&temp_path, identity);
        return Err(io_error("lock takeover temporary", &temp_path, source));
    }
    if let Err(source) = write_takeover_owner(&mut file, path, owner) {
        cleanup_prepared(&temp_path, identity);
        return Err(io_error("write takeover temporary", &temp_path, source));
    }
    if let Err(error) = ensure_path_identity(&temp_path, identity) {
        cleanup_prepared(&temp_path, identity);
        return Err(error);
    }

    Ok(PreparedLock {
        path: temp_path,
        owner,
        identity,
        file,
    })
}

fn validate_transition(
    lock_path: &Path,
    prepared: &PreparedLock,
    stale_file: &File,
    stale_identity: FileIdentity,
) -> Result<(), LockError> {
    ensure_path_identity(lock_path, prepared.identity)?;
    if regular_path_identity(&prepared.path)? != stale_identity
        || regular_file_identity(stale_file, lock_path)? != stale_identity
    {
        return Err(lock_changed_error(lock_path));
    }
    Ok(())
}

fn remove_displaced_stale(path: &Path, expected: FileIdentity) -> Result<(), LockError> {
    if regular_path_identity(path)? != expected {
        return Err(lock_changed_error(path));
    }
    fs::remove_file(path).map_err(|source| io_error("remove displaced stale", path, source))
}

fn rollback_transition(
    lock_path: &Path,
    temp_path: &Path,
    prepared_identity: FileIdentity,
    original_error: LockError,
) -> LockError {
    if regular_path_identity(lock_path).ok() == Some(prepared_identity)
        && fs::symlink_metadata(temp_path).is_ok()
    {
        if let Err(source) = exchange_paths(lock_path, temp_path) {
            cleanup_prepared(lock_path, prepared_identity);
            return io_error("roll back lock exchange", lock_path, source);
        }
        if regular_path_identity(temp_path).ok() == Some(prepared_identity) {
            if let Err(source) = fs::remove_file(temp_path) {
                return io_error("remove rolled-back temporary", temp_path, source);
            }
        }
        return original_error;
    }

    cleanup_prepared(lock_path, prepared_identity);
    cleanup_prepared(temp_path, prepared_identity);
    original_error
}

fn cleanup_prepared(path: &Path, identity: FileIdentity) {
    if verified_path_identity(path).ok() == Some(identity) {
        let _ = fs::remove_file(path);
    }
}

fn exchange_paths(first: &Path, second: &Path) -> io::Result<()> {
    let first = CString::new(first.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock path contains NUL"))?;
    let second = CString::new(second.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock path contains NUL"))?;

    // SAFETY: both C strings are NUL-terminated and remain alive for the syscall.
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
        Err(io::Error::last_os_error())
    }
}

fn next_temp_path(lock_path: &Path) -> PathBuf {
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut path = lock_path.as_os_str().to_os_string();
    path.push(format!(".swap.{}.{sequence}", std::process::id()));
    PathBuf::from(path)
}

fn read_owner_from_file(file: &mut File, path: &Path) -> Result<OwnerRecord, LockError> {
    read_record_from_file(file, path).map(|(_, owner)| owner)
}

fn read_record_from_file(
    file: &mut File,
    path: &Path,
) -> Result<(Vec<u8>, OwnerRecord), LockError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("read", path, source))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|source| io_error("read", path, source))?;
    let text = std::str::from_utf8(&contents).map_err(|_| LockError::Unverifiable {
        path: path.to_owned(),
        reason: "owner record is not valid UTF-8".to_owned(),
    })?;
    let owner = parse_owner_record(text).map_err(|reason| LockError::Unverifiable {
        path: path.to_owned(),
        reason,
    })?;
    Ok((contents, owner))
}

fn write_owner(file: &mut File, owner: OwnerRecord) -> io::Result<()> {
    replace_contents(
        file,
        format!("{} {}\n", owner.pid, owner.start_time).as_bytes(),
    )
}

fn write_takeover_owner(file: &mut File, path: &Path, owner: OwnerRecord) -> io::Result<()> {
    #[cfg(test)]
    if take_injected_write_failure(path) {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"partial")?;
        return Err(io::Error::other("injected takeover write failure"));
    }

    write_owner(file, owner)
}

fn replace_contents(file: &mut File, contents: &[u8]) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(contents)?;
    file.sync_data()
}

fn parse_owner_record(contents: &str) -> Result<OwnerRecord, String> {
    let mut fields = contents.split_whitespace();
    let pid = fields
        .next()
        .ok_or_else(|| "owner PID is missing".to_owned())?
        .parse::<u32>()
        .map_err(|_| "owner PID is invalid".to_owned())?;
    let start_time = fields
        .next()
        .ok_or_else(|| "owner start time is missing".to_owned())?
        .parse::<u64>()
        .map_err(|_| "owner start time is invalid".to_owned())?;
    if fields.next().is_some() {
        return Err("owner record has unexpected fields".to_owned());
    }

    Ok(OwnerRecord { pid, start_time })
}

fn current_owner() -> Result<OwnerRecord, LockError> {
    let pid = std::process::id();
    let proc_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let contents = fs::read_to_string(&proc_path).map_err(|source| LockError::Io {
        operation: "read process identity from",
        path: proc_path.clone(),
        source,
    })?;
    let start_time =
        parse_proc_start_time(&contents).map_err(|reason| LockError::Unverifiable {
            path: proc_path,
            reason,
        })?;

    Ok(OwnerRecord { pid, start_time })
}

fn process_start_time(pid: u32, lock_path: &Path) -> Result<Option<u64>, LockError> {
    let proc_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let contents = match fs::read_to_string(&proc_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LockError::Unverifiable {
                path: lock_path.to_owned(),
                reason: format!("cannot read {}: {error}", proc_path.display()),
            });
        }
    };
    parse_proc_start_time(&contents)
        .map(Some)
        .map_err(|reason| LockError::Unverifiable {
            path: lock_path.to_owned(),
            reason: format!("invalid {}: {reason}", proc_path.display()),
        })
}

pub(crate) fn parse_proc_start_time(contents: &str) -> Result<u64, String> {
    let close = contents
        .rfind(')')
        .ok_or_else(|| "process name has no closing parenthesis".to_owned())?;
    let fields = contents
        .get(close + 1..)
        .ok_or_else(|| "process record ends after its name".to_owned())?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| "start-time field 22 is missing".to_owned())?
        .parse::<u64>()
        .map_err(|_| "start-time field 22 is invalid".to_owned())
}

fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of this call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        return Err(error);
    }
}

fn lock_exclusive(file: &File) -> io::Result<()> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of this call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn ensure_path_identity(path: &Path, expected: FileIdentity) -> Result<(), LockError> {
    if verified_path_identity(path)? != expected {
        return Err(lock_changed_error(path));
    }
    Ok(())
}

fn verified_path_identity(path: &Path) -> Result<FileIdentity, LockError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error("inspect", path, source))?;
    verified_metadata_identity(&metadata, path)
}

fn verified_file_identity(file: &File, path: &Path) -> Result<FileIdentity, LockError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect", path, source))?;
    verified_metadata_identity(&metadata, path)
}

fn regular_path_identity(path: &Path) -> Result<FileIdentity, LockError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error("inspect", path, source))?;
    regular_metadata_identity(&metadata, path)
}

fn regular_file_identity(file: &File, path: &Path) -> Result<FileIdentity, LockError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect", path, source))?;
    regular_metadata_identity(&metadata, path)
}

fn regular_metadata_identity(
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<FileIdentity, LockError> {
    if !metadata.is_file() {
        return Err(LockError::Unverifiable {
            path: path.to_owned(),
            reason: "lock is not a regular file".to_owned(),
        });
    }
    Ok(FileIdentity::from(metadata))
}

fn verified_metadata_identity(
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<FileIdentity, LockError> {
    if !metadata.is_file() {
        return Err(LockError::Unverifiable {
            path: path.to_owned(),
            reason: "lock is not a regular file".to_owned(),
        });
    }
    if metadata.nlink() != 1 {
        return Err(LockError::Unverifiable {
            path: path.to_owned(),
            reason: "lock has multiple hard links".to_owned(),
        });
    }
    Ok(FileIdentity::from(metadata))
}

fn open_existing_error(path: &Path, source: io::Error) -> LockError {
    if source.raw_os_error() == Some(libc::ELOOP) {
        LockError::Unverifiable {
            path: path.to_owned(),
            reason: "lock is a symbolic link".to_owned(),
        }
    } else {
        io_error("open", path, source)
    }
}

fn lock_changed_error(path: &Path) -> LockError {
    LockError::Unverifiable {
        path: path.to_owned(),
        reason: "lock path changed during acquisition".to_owned(),
    }
}

#[cfg(test)]
pub(crate) fn set_stale_recovery_hook(hook: Option<StaleRecoveryHook>) {
    *STALE_RECOVERY_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

#[cfg(test)]
pub(crate) fn inject_takeover_write_failure(path: Option<PathBuf>) {
    *TAKEOVER_WRITE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
}

#[cfg(test)]
pub(crate) fn set_takeover_transition_hook(hook: Option<StaleRecoveryHook>) {
    *TAKEOVER_TRANSITION_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

#[cfg(test)]
pub(crate) fn inject_post_transition_failure(path: Option<PathBuf>) {
    *POST_TRANSITION_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
}

#[cfg(test)]
fn take_injected_write_failure(path: &Path) -> bool {
    let mut failure = TAKEOVER_WRITE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.as_deref() == Some(path) {
        failure.take();
        true
    } else {
        false
    }
}

#[cfg(test)]
fn run_takeover_transition_hook(path: &Path) {
    let hook = TAKEOVER_TRANSITION_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
fn run_takeover_transition_hook(_path: &Path) {}

#[cfg(test)]
fn injected_post_transition_error(path: &Path) -> Result<(), LockError> {
    let mut failure = POST_TRANSITION_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.as_deref() == Some(path) {
        failure.take();
        Err(LockError::Unverifiable {
            path: path.to_owned(),
            reason: "injected post-transition validation failure".to_owned(),
        })
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn injected_post_transition_error(_path: &Path) -> Result<(), LockError> {
    Ok(())
}

#[cfg(test)]
fn run_stale_recovery_hook(path: &Path) {
    let hook = STALE_RECOVERY_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
fn run_stale_recovery_hook(_path: &Path) {}

fn remove_created_if_unchanged(path: &Path, identity: FileIdentity) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if FileIdentity::from(&metadata) == identity {
        let _ = fs::remove_file(path);
    }
}

fn lock_path(output: &Path) -> PathBuf {
    let mut path = OsString::from(output.as_os_str());
    path.push(".rustypac.lock");
    PathBuf::from(path)
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> LockError {
    LockError::Io {
        operation,
        path: path.to_owned(),
        source,
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
