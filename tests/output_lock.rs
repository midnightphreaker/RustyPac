#[path = "../src/lock.rs"]
mod lock;

use std::fs;
use std::path::{Path, PathBuf};

use lock::{parse_proc_start_time, LockError, OutputLock};
use tempfile::tempdir;

fn lock_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".rustypac.lock");
    PathBuf::from(path)
}

fn current_start_time() -> u64 {
    let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
    parse_proc_start_time(&stat).unwrap()
}

#[test]
fn first_acquisition_creates_the_output_specific_owner_record() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);

    let owner = OutputLock::acquire(&output).unwrap();

    assert!(path.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{} {}\n", std::process::id(), current_start_time())
    );

    drop(owner);
    assert!(!path.exists());
}

#[test]
fn live_owner_rejects_a_duplicate_acquisition() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let owner = OutputLock::acquire(&output).unwrap();

    let error = OutputLock::acquire(&output).unwrap_err();

    assert!(matches!(
        error,
        LockError::Held { pid } if pid == std::process::id()
    ));
    drop(owner);
}

#[test]
fn missing_pid_record_is_recovered_as_stale() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    fs::write(&path, format!("{} 1\n", u32::MAX)).unwrap();

    let owner = OutputLock::acquire(&output).unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{} {}\n", std::process::id(), current_start_time())
    );
    drop(owner);
    assert!(!path.exists());
}

#[test]
fn reused_pid_with_a_different_start_time_is_recovered_as_stale() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    let stale_start_time = current_start_time().wrapping_add(1);
    fs::write(
        &path,
        format!("{} {stale_start_time}\n", std::process::id()),
    )
    .unwrap();

    let owner = OutputLock::acquire(&output).unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{} {}\n", std::process::id(), current_start_time())
    );
    drop(owner);
    assert!(!path.exists());
}

#[test]
fn drop_preserves_a_replaced_lock_even_when_the_record_was_copied() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    let owner = OutputLock::acquire(&output).unwrap();
    let copied_record = fs::read(&path).unwrap();

    fs::remove_file(&path).unwrap();
    fs::write(&path, &copied_record).unwrap();
    drop(owner);

    assert_eq!(fs::read(&path).unwrap(), copied_record);
}

#[test]
fn drop_preserves_a_non_owned_record_written_into_the_original_lock() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    let owner = OutputLock::acquire(&output).unwrap();
    let replacement = format!("{} {}\n", std::process::id(), current_start_time() + 1);

    fs::write(&path, &replacement).unwrap();
    drop(owner);

    assert_eq!(fs::read_to_string(&path).unwrap(), replacement);
}

#[test]
fn malformed_lock_is_reported_and_left_untouched() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    let malformed = b"not a verifiable owner record\n";
    fs::write(&path, malformed).unwrap();

    let error = OutputLock::acquire(&output).unwrap_err();

    assert!(matches!(error, LockError::Unverifiable { .. }));
    assert_eq!(fs::read(&path).unwrap(), malformed);
}

#[test]
fn proc_stat_parser_uses_field_22_after_the_final_closing_parenthesis() {
    let stat = "4321 (worker name (nested) parens) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20";

    assert_eq!(parse_proc_start_time(stat).unwrap(), 987_654);
}
