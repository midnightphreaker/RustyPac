#[path = "../src/lock.rs"]
mod lock;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use lock::{parse_proc_start_time, set_stale_recovery_hook, LockError, OutputLock};
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

#[derive(Default)]
struct RecoveryGateState {
    arrivals: usize,
    release_second: bool,
}

#[test]
fn concurrent_stale_recovery_has_one_protected_winner() {
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.part");
    let path = lock_path(&output);
    fs::write(&path, format!("{} 1\n", u32::MAX)).unwrap();

    let gate = Arc::new((Mutex::new(RecoveryGateState::default()), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    let hook_path = path.clone();
    set_stale_recovery_hook(Some(Arc::new(move |candidate| {
        if candidate != hook_path {
            return;
        }

        let (state, changed) = &*hook_gate;
        let mut state = state.lock().unwrap();
        state.arrivals += 1;
        if state.arrivals == 1 {
            changed.notify_all();
            while state.arrivals < 2 {
                state = changed.wait(state).unwrap();
            }
        } else {
            changed.notify_all();
            while !state.release_second {
                state = changed.wait(state).unwrap();
            }
        }
    })));

    let mut recoverers = Vec::new();
    for _ in 0..2 {
        let output = output.clone();
        let gate = Arc::clone(&gate);
        recoverers.push(thread::spawn(move || {
            let result = OutputLock::acquire(&output);
            let (state, changed) = &*gate;
            let mut state = state.lock().unwrap();
            state.release_second = true;
            changed.notify_all();
            result
        }));
    }

    let results: Vec<_> = recoverers
        .into_iter()
        .map(|recoverer| recoverer.join().unwrap())
        .collect();
    set_stale_recovery_hook(None);
    let mut winners: Vec<_> = results.into_iter().filter_map(Result::ok).collect();

    assert_eq!(winners.len(), 1, "only one stale recoverer may own output");
    assert!(matches!(
        OutputLock::acquire(&output),
        Err(LockError::Held { .. })
    ));
    assert!(path.exists(), "winner's lock record must remain linked");

    drop(winners.pop());
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
