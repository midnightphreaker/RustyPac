#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Output, Stdio};

use config::{apply, plan_disable, plan_enable, ConfigState, DisableChoice};
use tempfile::tempdir;

const ACTIVE: &str = "XferCommand = /usr/local/bin/RustyPac %u %o";

fn run_cli(path: &std::path::Path, argument: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_RustyPac"))
        .arg(argument)
        .env("RUSTYPAC_PACMAN_CONF", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
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
fn enabling_when_absent_appends_the_command_without_rewriting_existing_bytes() {
    let before = b"[options]\nArchitecture = auto\nColor\n";

    let plan = plan_enable(before, "/usr/local/bin/RustyPac").unwrap();

    assert_eq!(
        plan.contents(),
        b"[options]\nArchitecture = auto\nColor\nXferCommand = /usr/local/bin/RustyPac %u %o\n"
    );
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
fn enable_rejection_accepts_only_y_and_does_not_write() {
    for answer in ["\n", "n\n", "yes\n", "Y \n"] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("pacman.conf");
        let original = b"[options]\nColor\n";
        fs::write(&path, original).unwrap();

        let output = run_cli(&path, "--enable", answer);

        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read(&path).unwrap(), original);
        if unsafe { libc::geteuid() } != 0 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("sudo"));
        }
    }
}

#[test]
fn enable_accepts_lowercase_y() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    fs::write(&path, b"[options]\nColor\n").unwrap();

    let output = run_cli(&path, "--enable", "y\n");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(ACTIVE));
}

#[test]
fn disable_rejection_accepts_only_y_and_does_not_write() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pacman.conf");
    let original = format!("[options]\n{ACTIVE}\nColor\n");
    fs::write(&path, original.as_bytes()).unwrap();

    let output = run_cli(&path, "--disable", "yes\n");

    assert!(output.status.success(), "{output:?}");
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

        let output = run_cli(&path, "--disable", &format!("Y\n{choice}"));

        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(
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

        let output = run_cli(&path, "--disable", "Y\n");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
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

    let output = run_cli(&path, "--enable", "Y\n");

    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(String::from_utf8_lossy(&output.stderr).contains("sudo"));
}
