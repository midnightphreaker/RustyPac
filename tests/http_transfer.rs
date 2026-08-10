#[allow(dead_code)]
#[path = "../src/download.rs"]
mod download;
#[allow(dead_code)]
#[path = "support/http_server.rs"]
mod http_server;
#[allow(dead_code)]
#[path = "../src/lock.rs"]
mod lock;
#[allow(dead_code)]
#[path = "../src/progress.rs"]
mod progress;
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use download::{control_channel, run, terminal_boundary_control_channel, DownloadOutcome};
use http_server::{HttpServer, RecordedRequest, ResponseMode, ServerConfig};
use render::{Renderer, TerminalInfo};
use tempfile::tempdir;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DARK_GRAY: &str = "\x1b[90m";

#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl CapturedWriter {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("UTF-8 output")
    }
}

impl Write for CapturedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn sidecar_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".bytehaul");
    PathBuf::from(path)
}

fn observed_range_starts(server: &HttpServer) -> BTreeSet<u64> {
    server.requests().iter().filter_map(range_start).collect()
}

fn range_start(request: &RecordedRequest) -> Option<u64> {
    request
        .header("range")?
        .strip_prefix("bytes=")?
        .split_once('-')?
        .0
        .parse::<u64>()
        .ok()
}

async fn download_once(url: &str, output: &Path) -> (DownloadOutcome, String) {
    let writer = CapturedWriter::default();
    let capture = writer.clone();
    let mut renderer = Renderer::new(writer, || TerminalInfo::terminal(160), || Duration::ZERO);
    let (_control, events) = control_channel();
    let outcome = run(url, output, events, &mut renderer).await;
    (outcome, capture.text())
}

async fn download_with_control_queued_at_terminal(
    url: &str,
    output: &Path,
) -> (DownloadOutcome, String) {
    let writer = CapturedWriter::default();
    let capture = writer.clone();
    let mut renderer = Renderer::new(writer, || TerminalInfo::terminal(160), || Duration::ZERO);
    let (control, events) = terminal_boundary_control_channel();
    control.interrupt();
    let outcome = run(url, output, events, &mut renderer).await;
    (outcome, capture.text())
}

async fn create_interrupted_checkpoint(expected: &[u8], output: &Path) {
    let slow_server = HttpServer::spawn(
        ServerConfig::new("/resume", expected.to_vec(), ResponseMode::Ranges)
            .throttled(8 * 1024, Duration::from_millis(8)),
    );
    let writer = CapturedWriter::default();
    let mut renderer = Renderer::new(writer, || TerminalInfo::terminal(160), || Duration::ZERO);
    let (control, events) = control_channel();
    let url = slow_server.url("/resume");
    let task_output = output.to_owned();
    let task = tokio::spawn(async move { run(&url, &task_output, events, &mut renderer).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if output.exists() && !slow_server.requests().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "download did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    control.interrupt();
    assert_eq!(task.await.unwrap(), DownloadOutcome::Interrupted);
    assert!(sidecar_path(output).exists());
}

#[tokio::test]
async fn range_server_writes_exact_output_and_cleans_resume_state() {
    let expected = body(11 * 1024 * 1024 + 123);
    let server = HttpServer::spawn(ServerConfig::new(
        "/package",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.pkg.tar.zst.part");

    let (outcome, rendered) = download_once(&server.url("/package"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    let range_starts = observed_range_starts(&server);
    assert!(range_starts.contains(&0));
    assert!(range_starts.iter().any(|start| *start > 0));
    assert!(range_starts.len() > 1);
    assert!(rendered.contains(GREEN), "completion must render green");
}

#[tokio::test]
async fn no_range_server_falls_back_to_one_correct_stream() {
    let expected = body(384 * 1024);
    let server = HttpServer::spawn(ServerConfig::new(
        "/database",
        expected.clone(),
        ResponseMode::IgnoreRanges,
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("repo.db.part");

    let (outcome, _) = download_once(&server.url("/database"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    assert!(server
        .requests()
        .iter()
        .any(|request| request.header("range").is_some()));
}

#[tokio::test]
async fn ordinary_404_is_a_red_failure() {
    let server = HttpServer::spawn(ServerConfig::new(
        "/missing-package",
        Vec::new(),
        ResponseMode::Status(404),
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("missing.pkg.tar.zst.part");

    let (outcome, rendered) = download_once(&server.url("/missing-package"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Failed);
    assert!(rendered.contains(RED), "ordinary failure must render red");
    assert!(!rendered.contains(DARK_GRAY));
}

#[tokio::test]
async fn paired_database_signature_request_and_part_output_are_dark_gray_skips() {
    for status in [404, 410] {
        let server = HttpServer::spawn(ServerConfig::new(
            "/repo.db.sig",
            Vec::new(),
            ResponseMode::Status(status),
        ));
        let directory = tempdir().unwrap();
        let output = directory.path().join("repo.db.sig.part");

        let (outcome, rendered) = download_once(&server.url("/repo.db.sig"), &output).await;

        assert_eq!(outcome, DownloadOutcome::SkippedDatabaseSignature);
        assert!(rendered.contains(DARK_GRAY), "skip must render dark gray");
        assert!(rendered.contains("⛓️‍💥"));
    }
}

#[tokio::test]
async fn database_signature_skip_requires_matching_request_and_part_output() {
    for (request_path, output_name) in [
        ("/ordinary.db", "repo.db.sig.part"),
        ("/repo.db.sig", "ordinary.db.part"),
        ("/repo.db.sig", "repo.db.sig"),
    ] {
        let server = HttpServer::spawn(ServerConfig::new(
            request_path,
            Vec::new(),
            ResponseMode::Status(404),
        ));
        let directory = tempdir().unwrap();
        let output = directory.path().join(output_name);

        let (outcome, rendered) = download_once(&server.url(request_path), &output).await;

        assert_eq!(
            outcome,
            DownloadOutcome::Failed,
            "{request_path} -> {output_name} must hard-fail"
        );
        assert!(rendered.contains(RED));
        assert!(!rendered.contains(DARK_GRAY));
    }
}

#[tokio::test]
async fn package_signature_404_remains_a_failure() {
    let server = HttpServer::spawn(ServerConfig::new(
        "/package.pkg.tar.zst.sig",
        Vec::new(),
        ResponseMode::Status(404),
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("package.pkg.tar.zst.sig.part");

    let (outcome, rendered) = download_once(&server.url("/package.pkg.tar.zst.sig"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Failed);
    assert!(rendered.contains(RED));
}

#[tokio::test]
async fn unknown_length_is_unavailable_while_active_and_measured_on_completion() {
    let expected = body(96 * 1024);
    let server = HttpServer::spawn(
        ServerConfig::new("/unknown", expected.clone(), ResponseMode::UnknownLength)
            .throttled(4 * 1024, Duration::from_millis(2)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("unknown.db.part");

    let (outcome, rendered) = download_once(&server.url("/unknown"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(
        rendered.contains("N/A"),
        "active unknown fields must be unavailable"
    );
    assert!(
        rendered.contains("100%"),
        "completion must use measured size"
    );
    assert!(rendered.contains(GREEN));
    assert!(!sidecar_path(&output).exists());
}

#[tokio::test]
async fn empty_success_response_is_rejected() {
    let server = HttpServer::spawn(ServerConfig::new(
        "/empty",
        Vec::new(),
        ResponseMode::IgnoreRanges,
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("empty.part");

    let (outcome, rendered) = download_once(&server.url("/empty"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Failed);
    assert!(rendered.contains(RED));
}

#[tokio::test]
async fn queued_control_at_success_boundary_preserves_completed_outcome() {
    let expected = body(64 * 1024);
    let server = HttpServer::spawn(ServerConfig::new(
        "/terminal-success",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("terminal-success.part");

    let (outcome, rendered) =
        download_with_control_queued_at_terminal(&server.url("/terminal-success"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    assert!(rendered.contains(GREEN));
}

#[tokio::test]
async fn queued_control_at_error_boundary_preserves_hard_failure() {
    let server = HttpServer::spawn(ServerConfig::new(
        "/terminal-error",
        Vec::new(),
        ResponseMode::Status(404),
    ));
    let directory = tempdir().unwrap();
    let output = directory.path().join("terminal-error.part");

    let (outcome, rendered) =
        download_with_control_queued_at_terminal(&server.url("/terminal-error"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Failed);
    assert!(rendered.contains(RED));
}

async fn interrupt_then_complete(use_alternate_url: bool) {
    let expected = body(768 * 1024);
    let slow_server = HttpServer::spawn(
        ServerConfig::new("/resume", expected.clone(), ResponseMode::Ranges)
            .throttled(8 * 1024, Duration::from_millis(8)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("resumable.pkg.part");
    let writer = CapturedWriter::default();
    let mut renderer = Renderer::new(writer, || TerminalInfo::terminal(160), || Duration::ZERO);
    let (control, events) = control_channel();
    let url = slow_server.url("/resume");
    let task_output = output.clone();
    let task = tokio::spawn(async move { run(&url, &task_output, events, &mut renderer).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if output.exists() && !slow_server.requests().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "download did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    control.interrupt();

    assert_eq!(task.await.unwrap(), DownloadOutcome::Interrupted);
    let sidecar = sidecar_path(&output);
    assert!(
        output.exists(),
        "interruption must retain the partial output"
    );
    let control_bytes = fs::read(&sidecar).expect("interruption must retain resume state");
    assert!(
        control_bytes.len() > 16,
        "resume sidecar must contain a payload"
    );
    assert_eq!(&control_bytes[..4], &0x4259_4845u32.to_le_bytes());
    let requests_before_restart = slow_server.requests().len();

    let alternate_server;
    let completion_url = if use_alternate_url {
        alternate_server = HttpServer::spawn(ServerConfig::new(
            "/resume",
            expected.clone(),
            ResponseMode::Ranges,
        ));
        alternate_server.url("/resume")
    } else {
        slow_server.url("/resume")
    };

    let (outcome, _) = download_once(&completion_url, &output).await;
    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar.exists());
    if !use_alternate_url {
        let requests = slow_server.requests();
        assert!(
            requests[requests_before_restart..]
                .iter()
                .filter_map(range_start)
                .any(|start| start > 0),
            "same-URL restart must request a nonzero resume offset"
        );
    }
}

#[tokio::test]
async fn interrupted_known_length_download_resumes_and_cleans_up() {
    interrupt_then_complete(false).await;
}

#[tokio::test]
async fn changed_url_after_interruption_still_completes_correctly() {
    interrupt_then_complete(true).await;
}

#[tokio::test]
async fn failed_alternate_mirror_preserves_resume_state_for_later_mirror() {
    let expected = body(768 * 1024);
    let directory = tempdir().unwrap();
    let output = directory.path().join("mirror-fallback.pkg.part");
    create_interrupted_checkpoint(&expected, &output).await;

    let sidecar = sidecar_path(&output);
    let partial_before_failure = fs::read(&output).unwrap();
    let checkpoint_before_failure = fs::read(&sidecar).unwrap();
    assert!(!partial_before_failure.is_empty());
    assert!(checkpoint_before_failure.len() > 16);

    let failing_server = HttpServer::spawn(ServerConfig::new(
        "/resume",
        Vec::new(),
        ResponseMode::Status(404),
    ));
    let failed = Command::new(env!("CARGO_BIN_EXE_RustyPac"))
        .arg(failing_server.url("/resume"))
        .arg(&output)
        .output()
        .unwrap();

    assert!(!failed.status.success());
    assert_eq!(fs::read(&output).unwrap(), partial_before_failure);
    assert_eq!(fs::read(&sidecar).unwrap(), checkpoint_before_failure);
    assert_eq!(
        fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
        0o600,
        "restored checkpoint must use private permissions"
    );

    let working_server = HttpServer::spawn(ServerConfig::new(
        "/resume",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let (outcome, _) = download_once(&working_server.url("/resume"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Completed);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(
        working_server
            .requests()
            .iter()
            .filter_map(range_start)
            .any(|start| start > 0),
        "later working mirror must resume from a nonzero offset"
    );
    assert!(!sidecar.exists());
}

#[tokio::test]
async fn failed_transfer_that_replaces_output_does_not_restore_stale_checkpoint() {
    let expected = body(768 * 1024);
    let directory = tempdir().unwrap();
    let output = directory.path().join("replaced.pkg.part");
    create_interrupted_checkpoint(&expected, &output).await;
    let sidecar = sidecar_path(&output);
    let partial_before_failure = fs::read(&output).unwrap();
    let checkpoint_before_failure = fs::read(&sidecar).unwrap();

    let failing_server = HttpServer::spawn(ServerConfig::new(
        "/replacement",
        body(128 * 1024),
        ResponseMode::TruncatedRanges,
    ));
    let (outcome, _) = download_once(&failing_server.url("/replacement"), &output).await;

    assert_eq!(outcome, DownloadOutcome::Failed);
    assert_ne!(fs::read(&output).unwrap(), partial_before_failure);
    assert!(
        !sidecar.exists() || fs::read(&sidecar).unwrap() != checkpoint_before_failure,
        "stale checkpoint must not be restored for replacement output bytes"
    );
}

#[test]
fn binary_returns_zero_only_for_verified_completion() {
    let expected = body(32 * 1024);
    let success_server = HttpServer::spawn(ServerConfig::new(
        "/success",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let directory = tempdir().unwrap();
    let success_output = directory.path().join("success.part");

    let success = Command::new(env!("CARGO_BIN_EXE_RustyPac"))
        .arg(success_server.url("/success"))
        .arg(&success_output)
        .output()
        .unwrap();

    assert!(success.status.success());
    assert_eq!(fs::read(success_output).unwrap(), expected);

    let failure_server = HttpServer::spawn(ServerConfig::new(
        "/missing",
        Vec::new(),
        ResponseMode::Status(404),
    ));
    let failure_output = directory.path().join("missing.part");
    let failure = Command::new(env!("CARGO_BIN_EXE_RustyPac"))
        .arg(failure_server.url("/missing"))
        .arg(failure_output)
        .output()
        .unwrap();

    assert!(!failure.status.success());
}
