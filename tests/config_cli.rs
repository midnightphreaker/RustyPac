#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/config_interaction.rs"]
mod config_interaction;

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Cursor, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use config::{apply, plan_disable, plan_enable, ApplyOutcome, ConfigState, DisableChoice};
use config_interaction::RunStatus;
use tempfile::tempdir;

const ACTIVE: &str = "XferCommand = /usr/local/bin/RustyPac %u %o";
const PLANNED: &[u8] = b"[options]\nColor\nXferCommand = /usr/local/bin/RustyPac %u %o\n";
static CLEANUP_RACE_TEST: Mutex<()> = Mutex::new(());

fn edit_lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.rustypac-edit.lock",
        path.file_name().unwrap().to_string_lossy()
    ))
}

fn staging_entries(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".rustypac-stage-")
        })
        .collect()
}

fn run_cli(path: &Path, argument: &str, input: &str) -> (RunStatus, Vec<u8>) {
    let mut input = Cursor::new(input.as_bytes());
    let mut output = Vec::new();
    let status = match argument {
        "--enable" => config_interaction::run_enable(path, &mut input, &mut output),
        "--disable" => config_interaction::run_disable(path, &mut input, &mut output),
        _ => unreachable!(),
    };
    (status, output)
}

#[test]
fn production_path_is_fixed_while_interactions_require_an_explicit_fixture() {
    assert_eq!(
        config_interaction::production_config_path(),
        Path::new("/etc/pacman.conf")
    );

    let directory = tempdir().unwrap();
    let fixture = directory.path().join("pacman.conf");
    fs::write(&fixture, b"[options]\nColor\n").unwrap();
    let (status, _) = run_cli(&fixture, "--enable", "y\n");

    assert_eq!(status, RunStatus::Success);
    assert!(fs::read_to_string(fixture).unwrap().contains(ACTIVE));
}

fn assert_bytes_around_relevant_line_are_unchanged(before: &[u8], after: &[u8]) {
    let prefix = b"[options]\nArchitecture = auto\n";
    let suffix = b"\nColor\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
    assert!(before.starts_with(prefix));
    assert!(before.ends_with(suffix));
    assert!(after.starts_with(prefix));
    assert!(after.ends_with(suffix));
}

#[test]
fn enabling_an_active_command_plans_no_write() {
    let contents = format!("[options]\n{ACTIVE}\nColor\n");

    let plan = plan_enable(contents.as_bytes(), "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(plan.initial_state(), &ConfigState::Active);
    assert!(!plan.changed());
    assert_eq!(plan.contents(), contents.as_bytes());
}

#[test]
fn enabling_a_commented_command_uncomments_only_that_line() {
    let before = b"[options]\r\nArchitecture = auto\r\n# XferCommand = /usr/local/bin/RustyPac %u %o\r\nColor\r\n";

    let plan = plan_enable(before, "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(
        plan.contents(),
        b"[options]\r\nArchitecture = auto\r\nXferCommand = /usr/local/bin/RustyPac %u %o\r\nColor\r\n"
    );
}

#[test]
fn enabling_when_absent_inserts_in_options_before_repository_sections() {
    let before = b"[options]\nArchitecture = auto\nColor\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n";

    let plan = plan_enable(before, "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(
        plan.contents(),
        b"[options]\nArchitecture = auto\nColor\n\nXferCommand = /usr/local/bin/RustyPac %u %o\n[core]\nInclude = /etc/pacman.d/mirrorlist\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n"
    );
}

#[test]
fn absent_enable_rejects_missing_or_ambiguous_options_sections() {
    for contents in [
        b"[core]\nInclude = /etc/pacman.d/mirrorlist\n".as_slice(),
        b"[options]\nColor\n[options]\nArchitecture = auto\n".as_slice(),
    ] {
        assert!(
            plan_enable(contents, "/usr/local/bin/RustyPac").is_err(),
            "must reject {contents:?}"
        );
    }
}

#[test]
fn enabling_preserves_a_conflicting_downloader_and_unrelated_bytes() {
    let before = b"[options]\nArchitecture = auto\nXferCommand = /usr/bin/curl --continue-at - %u -o %o\nColor\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";

    let plan = plan_enable(before, "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(
        plan.initial_state(),
        &ConfigState::Conflicting {
            line: "XferCommand = /usr/bin/curl --continue-at - %u -o %o".to_owned()
        }
    );
    assert_eq!(
        plan.contents(),
        b"[options]\nArchitecture = auto\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl --continue-at - %u -o %o\nXferCommand = /usr/local/bin/RustyPac %u %o\nColor\n[core]\nInclude = /etc/pacman.d/mirrorlist\n"
    );
    assert_bytes_around_relevant_line_are_unchanged(before, plan.contents());
}

#[test]
fn enabling_reuses_a_preserved_downloader_and_commented_rustypac_line() {
    let before = b"## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n#XferCommand = /usr/local/bin/RustyPac %u %o\n";

    let plan = plan_enable(before, "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(
        plan.contents(),
        b"## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\nXferCommand = /usr/local/bin/RustyPac %u %o\n"
    );
}

#[test]
fn duplicated_or_malformed_relevant_lines_are_rejected() {
    for contents in [
        format!("{ACTIVE}\n{ACTIVE}\n"),
        format!("{ACTIVE}\n#{ACTIVE}\n"),
        "XferCommand /usr/bin/curl %u -o %o\n".to_owned(),
        "## Pre-RustyPac XferCommand ## not an XferCommand\n".to_owned(),
    ] {
        assert!(
            plan_enable(contents.as_bytes(), "/usr/local/bin/RustyPac").is_err(),
            "must reject {contents:?}"
        );
    }
}

#[test]
fn disabling_with_existing_choice_restores_the_preserved_command() {
    let before = format!(
        "[options]\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n{ACTIVE}\nColor\n"
    );

    let plan = plan_disable(before.as_bytes(), DisableChoice::Existing).unwrap();

    assert_eq!(plan.initial_state(), &ConfigState::Active);
    assert_eq!(
        plan.contents(),
        b"[options]\nXferCommand = /usr/bin/curl %u -o %o\n#XferCommand = /usr/local/bin/RustyPac %u %o\nColor\n"
    );
}

#[test]
fn restoring_preserved_command_keeps_payload_indentation_trailing_bytes_and_crlf() {
    let before = b"[options]\r\n  ## Pre-RustyPac XferCommand ## \tXferCommand = /usr/bin/curl %u -o %o  \t\r\nXferCommand = /usr/local/bin/RustyPac %u %o\r\nColor\r\n";

    let plan = plan_disable(before, DisableChoice::Existing).unwrap();

    assert_eq!(
        plan.contents(),
        b"[options]\r\n\tXferCommand = /usr/bin/curl %u -o %o  \t\r\n#XferCommand = /usr/local/bin/RustyPac %u %o\r\nColor\r\n"
    );
}

#[test]
fn disabling_with_default_choice_keeps_preserved_command_and_comments_only_rustypac() {
    let before = format!(
        "[options]\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n{ACTIVE}\nColor\n"
    );

    let plan = plan_disable(before.as_bytes(), DisableChoice::Default).unwrap();

    assert_eq!(
        plan.contents(),
        b"[options]\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n#XferCommand = /usr/local/bin/RustyPac %u %o\nColor\n"
    );
}

#[test]
fn apply_atomically_preserves_mode_owner_and_group() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    fs::write(&path, b"[options]\nColor\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&path).unwrap();
    let plan = plan_enable(&fs::read(&path).unwrap(), "/usr/local/bin/RustyPac").unwrap();

    apply(&path, plan).unwrap();

    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.mode() & 0o7777, 0o640);
    assert_eq!(after.uid(), before.uid());
    assert_eq!(after.gid(), before.gid());
    assert_eq!(
        fs::read(&path).unwrap(),
        b"[options]\nColor\nXferCommand = /usr/local/bin/RustyPac %u %o\n"
    );
}

#[test]
fn foreign_stable_edit_lock_is_rejected_before_configuration_mutation() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let lock_path = edit_lock_path(&path);
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    fs::write(&lock_path, b"foreign lock contents\n").unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();

    let result = apply(&path, plan);

    assert!(result.is_err(), "{result:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read(&lock_path).unwrap(), b"foreign lock contents\n");
    assert!(staging_entries(directory.path()).is_empty());
}

#[test]
fn stable_edit_lock_serializes_overlapping_edits() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    let first_plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let second_plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let hook_calls_for_hook = Arc::clone(&hook_calls);
    let release_for_hook = Arc::clone(&release_rx);
    config::set_apply_pre_commit_hook(Some(Arc::new(move |_| {
        let call = hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
        entered_tx.send(call).unwrap();
        if call == 0 {
            release_for_hook.lock().unwrap().recv().unwrap();
        }
    })));

    let first_path = path.clone();
    let first = thread::spawn(move || apply(&first_path, first_plan));
    assert_eq!(entered_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
    let second_path = path.clone();
    let second = thread::spawn(move || apply(&second_path, second_plan));
    let overlap = entered_rx.recv_timeout(Duration::from_millis(250)).is_ok();
    release_tx.send(()).unwrap();
    let first_result = first.join().unwrap();
    let second_result = second.join().unwrap();
    config::set_apply_pre_commit_hook(None);

    assert!(
        !overlap,
        "a second edit reached commit while the first held the lock"
    );
    assert!(first_result.is_ok(), "{first_result:?}");
    assert!(matches!(
        second_result,
        Err(config::ConfigError::ChangedDuringEdit)
    ));
    assert_eq!(fs::read(&path).unwrap(), PLANNED);
}

#[test]
fn stable_edit_lock_mode_is_enforced() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let lock_path = edit_lock_path(&path);
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    fs::write(&lock_path, b"RustyPac edit lock v1\n").unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();

    apply(&path, plan).unwrap();

    assert!(
        lock_path.exists(),
        "the stable lock must remain at its fixed path"
    );
    let metadata = fs::metadata(lock_path).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
}

#[test]
fn successful_apply_leaves_no_private_staging_directory() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();

    apply(&path, plan).unwrap();

    assert!(staging_entries(directory.path()).is_empty());
    let mut entries: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            OsString::from(".pacman.conf.rustypac-edit.lock"),
            OsString::from("pacman.conf")
        ]
    );
    assert_eq!(fs::read(&path).unwrap(), PLANNED);
}

#[test]
fn removed_displaced_temp_does_not_remove_validated_installed_configuration() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let expected_parent = directory.path().to_owned();
    let observed = Arc::new(Mutex::new(None));
    let hook_observed = Arc::clone(&observed);
    config::set_apply_post_validation_hook(Some(Arc::new(move |temporary| {
        if temporary.parent().and_then(Path::parent) == Some(expected_parent.as_path()) {
            *hook_observed.lock().unwrap() = Some(temporary.to_owned());
            fs::remove_file(temporary).unwrap();
        }
    })));

    let result = apply(&path, plan);
    config::set_apply_post_validation_hook(None);

    assert_eq!(result.unwrap(), ApplyOutcome::Applied);
    assert_eq!(fs::read(&path).unwrap(), PLANNED);
    let temporary = observed.lock().unwrap().clone().unwrap();
    assert!(!temporary.exists());
}

#[test]
fn replaced_displaced_temp_is_retained_without_rolling_back_validated_install() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    let foreign = b"foreign temporary bytes\n";
    fs::write(&path, original).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let expected_parent = directory.path().to_owned();
    let observed = Arc::new(Mutex::new(None));
    let hook_observed = Arc::clone(&observed);
    config::set_apply_post_validation_hook(Some(Arc::new(move |temporary| {
        if temporary.parent().and_then(Path::parent) == Some(expected_parent.as_path()) {
            *hook_observed.lock().unwrap() = Some(temporary.to_owned());
            let replacement = temporary.with_extension("foreign");
            fs::write(&replacement, foreign).unwrap();
            fs::rename(replacement, temporary).unwrap();
        }
    })));

    let result = apply(&path, plan);
    config::set_apply_post_validation_hook(None);

    let temporary = observed.lock().unwrap().clone().unwrap();
    assert_eq!(
        result.unwrap(),
        ApplyOutcome::AppliedWithCleanupPending {
            staging_directory: temporary.parent().unwrap().to_owned()
        }
    );
    assert_eq!(fs::read(&path).unwrap(), PLANNED);
    assert_eq!(fs::read(temporary).unwrap(), foreign);
}

#[test]
fn cleanup_boundary_replacement_is_not_unlinked_or_installed() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    let foreign = b"replacement at former check-unlink boundary\n";
    fs::write(&path, original).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let expected_parent = directory.path().to_owned();
    let guard_path = path.with_file_name(".pacman.conf.rustypac-cleanup.guard");
    let hooked_guard_path = guard_path.clone();
    config::set_apply_cleanup_boundary_hook(Some(Arc::new(move |temporary| {
        if temporary.parent().and_then(Path::parent) == Some(expected_parent.as_path()) {
            fs::write(&hooked_guard_path, foreign).unwrap();
        }
    })));

    let result = apply(&path, plan);
    config::set_apply_cleanup_boundary_hook(None);

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(&path).unwrap(), PLANNED);
    assert_eq!(fs::read(guard_path).unwrap(), foreign);
}

#[test]
fn changed_before_commit_returns_changed_without_losing_new_bytes() {
    let _serial = CLEANUP_RACE_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    let concurrent = b"[options]\nColor\nCheckSpace\n";
    fs::write(&path, original).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();
    let hooked_path = path.clone();
    config::set_apply_pre_commit_hook(Some(Arc::new(move |candidate| {
        if candidate == hooked_path {
            let replacement = candidate.with_extension("concurrent");
            fs::write(&replacement, concurrent).unwrap();
            fs::rename(replacement, candidate).unwrap();
        }
    })));

    let result = apply(&path, plan);
    config::set_apply_pre_commit_hook(None);

    assert!(matches!(
        result,
        Err(config::ConfigError::ChangedDuringEdit)
    ));
    assert_eq!(fs::read(&path).unwrap(), concurrent);
    assert!(staging_entries(directory.path()).is_empty());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn apply_rejects_symlink_without_changing_link_or_referent() {
    let directory = tempdir().unwrap();
    let referent = directory.path().join("real.conf");
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    fs::write(&referent, original).unwrap();
    std::os::unix::fs::symlink(&referent, &path).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();

    assert!(apply(&path, plan).is_err());

    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&referent).unwrap(), original);
}

#[test]
fn apply_rejects_non_regular_source_without_replacing_it() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    fs::create_dir(&path).unwrap();
    let plan = plan_enable(b"[options]\nColor\n", "/usr/local/bin/RustyPac").unwrap();

    assert!(apply(&path, plan).is_err());

    assert!(fs::symlink_metadata(&path).unwrap().is_dir());
}

#[test]
fn apply_rejects_hard_link_without_breaking_or_changing_aliases() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let alias = directory.path().join("pacman.alias");
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    fs::hard_link(&path, &alias).unwrap();
    let before = fs::metadata(&path).unwrap();
    let plan = plan_enable(original, "/usr/local/bin/RustyPac").unwrap();

    assert!(apply(&path, plan).is_err());

    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.ino(), before.ino());
    assert_eq!(fs::metadata(&alias).unwrap().ino(), before.ino());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read(&alias).unwrap(), original);
}

#[test]
fn enable_rejection_accepts_only_y_and_does_not_write() {
    for answer in ["\n", "n\n", "yes\n", "Y \n"] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pacman.conf");
        let original = b"[options]\nColor\n";
        fs::write(&path, original).unwrap();

        let (status, output) = run_cli(&path, "--enable", answer);

        assert_eq!(status, RunStatus::Success, "{output:?}");
        assert_eq!(fs::read(&path).unwrap(), original);
        if unsafe { libc::geteuid() } != 0 {
            assert!(String::from_utf8_lossy(&output).contains("sudo"));
        }
    }
}

#[test]
fn enable_accepts_lowercase_y() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    fs::write(&path, b"[options]\nColor\n").unwrap();

    let (status, output) = run_cli(&path, "--enable", "y\n");

    assert_eq!(status, RunStatus::Success, "{output:?}");
    assert!(String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(ACTIVE));
}

#[test]
fn disable_rejection_accepts_only_y_and_does_not_write() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = format!("[options]\n{ACTIVE}\nColor\n");
    fs::write(&path, original.as_bytes()).unwrap();

    let (status, output) = run_cli(&path, "--disable", "yes\n");

    assert_eq!(status, RunStatus::Success, "{output:?}");
    assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
}

#[test]
fn disable_e_restores_existing_while_other_responses_select_default() {
    for (choice, restored) in [("e\n", true), ("E\n", true), ("\n", false), ("x\n", false)] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pacman.conf");
        let original = format!(
            "[options]\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n{ACTIVE}\n"
        );
        fs::write(&path, original).unwrap();

        let (status, output) = run_cli(&path, "--disable", &format!("Y\n{choice}"));

        assert_eq!(status, RunStatus::Success, "{output:?}");
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains(
            "Revert to existing XferCommand [e] or to default pacman behaviour? [enter]"
        ));
        let changed = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            changed.contains("\nXferCommand = /usr/bin/curl %u -o %o\n"),
            restored
        );
        assert!(changed.contains("#XferCommand = /usr/local/bin/RustyPac %u %o"));
        assert_eq!(
            changed.contains("## Pre-RustyPac XferCommand ##"),
            !restored
        );
    }
}

#[test]
fn absent_and_disabled_commands_report_their_state_without_writing() {
    for (contents, expected) in [
        ("[options]\nColor\n", "RustyPac is absent"),
        (
            "[options]\n#XferCommand = /usr/local/bin/RustyPac %u %o\n",
            "RustyPac is already disabled",
        ),
    ] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pacman.conf");
        fs::write(&path, contents).unwrap();

        let (status, output) = run_cli(&path, "--disable", "Y\n");

        assert_eq!(status, RunStatus::Success, "{output:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        assert!(String::from_utf8_lossy(&output).contains(expected));
    }
}

#[test]
fn permission_denial_keeps_original_and_tells_non_root_user_to_use_sudo() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = b"[options]\nColor\n";
    fs::write(&path, original).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o555)).unwrap();

    let (status, output) = run_cli(&path, "--enable", "Y\n");

    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, RunStatus::Failure, "{output:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(String::from_utf8_lossy(&output).contains("sudo"));
}

struct FailsAfterFirstLine {
    first: Option<&'static [u8]>,
}

impl Read for FailsAfterFirstLine {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let length = available.len().min(buffer.len());
        buffer[..length].copy_from_slice(&available[..length]);
        self.consume(length);
        Ok(length)
    }
}

impl BufRead for FailsAfterFirstLine {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.first
            .ok_or_else(|| io::Error::other("injected second prompt failure"))
    }

    fn consume(&mut self, amount: usize) {
        if self.first.is_some_and(|line| amount >= line.len()) {
            self.first = None;
        }
    }
}

#[test]
fn second_prompt_read_error_fails_closed_without_writing() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = format!(
        "[options]\n## Pre-RustyPac XferCommand ## XferCommand = /usr/bin/curl %u -o %o\n{ACTIVE}\n"
    );
    fs::write(&path, original.as_bytes()).unwrap();
    let mut input = FailsAfterFirstLine {
        first: Some(b"Y\n"),
    };
    let mut output = Vec::new();

    let status = config_interaction::run_disable(&path, &mut input, &mut output);

    assert_eq!(status, RunStatus::Failure);
    assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    assert!(String::from_utf8_lossy(&output).contains("failed to read response"));
}
