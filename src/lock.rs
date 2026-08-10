use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct OutputLock {
    path: PathBuf,
    owner: OwnerRecord,
    identity: FileIdentity,
    _file: File,
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

#[derive(Debug)]
struct LockSnapshot {
    owner: OwnerRecord,
    identity: FileIdentity,
}

impl OutputLock {
    pub fn acquire(output: &Path) -> Result<Self, LockError> {
        let path = lock_path(output);
        let owner = current_owner()?;

        match create_lock(&path, owner) {
            Ok(lock) => Ok(lock),
            Err(CreateError::Exists) => {
                let snapshot = read_snapshot(&path)?;
                match process_start_time(snapshot.owner.pid, &path)? {
                    Some(start_time) if start_time == snapshot.owner.start_time => {
                        Err(LockError::Held {
                            pid: snapshot.owner.pid,
                        })
                    }
                    Some(_) | None => {
                        remove_stale_if_unchanged(&path, &snapshot)?;
                        match create_lock(&path, owner) {
                            Ok(lock) => Ok(lock),
                            Err(CreateError::Exists) => Err(LockError::Unverifiable {
                                path,
                                reason: "lock changed while recovering stale ownership".to_owned(),
                            }),
                            Err(CreateError::Io(error)) => Err(error),
                        }
                    }
                }
            }
            Err(CreateError::Io(error)) => Err(error),
        }
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        let Ok(snapshot) = read_snapshot(&self.path) else {
            return;
        };

        if snapshot.owner != self.owner || snapshot.identity != self.identity {
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
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
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
    let record = format!("{} {}\n", owner.pid, owner.start_time);

    if let Err(source) = file
        .write_all(record.as_bytes())
        .and_then(|()| file.sync_data())
    {
        remove_created_if_unchanged(path, identity);
        return Err(CreateError::Io(io_error("write", path, source)));
    }

    Ok(OutputLock {
        path: path.to_owned(),
        owner,
        identity,
        _file: file,
    })
}

fn read_snapshot(path: &Path) -> Result<LockSnapshot, LockError> {
    let mut file = File::open(path).map_err(|source| io_error("read", path, source))?;
    let identity = FileIdentity::from(
        &file
            .metadata()
            .map_err(|source| io_error("inspect", path, source))?,
    );
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| io_error("read", path, source))?;
    let owner = parse_owner_record(&contents).map_err(|reason| LockError::Unverifiable {
        path: path.to_owned(),
        reason,
    })?;

    Ok(LockSnapshot { owner, identity })
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

fn remove_stale_if_unchanged(path: &Path, expected: &LockSnapshot) -> Result<(), LockError> {
    let current = read_snapshot(path)?;
    if current.owner != expected.owner || current.identity != expected.identity {
        return Err(LockError::Unverifiable {
            path: path.to_owned(),
            reason: "lock changed while verifying stale ownership".to_owned(),
        });
    }

    fs::remove_file(path).map_err(|source| io_error("remove stale", path, source))
}

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
