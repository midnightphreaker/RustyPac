#[allow(dead_code)]
#[path = "support/http_server.rs"]
mod http_server;

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use http_server::{HttpServer, RecordedRequest, ResponseMode, ServerConfig};
use tempfile::tempdir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const RED: &[u8] = b"\x1b[31m";
const PAUSE_GATE_ENV: &str = "RUSTYPAC_TEST_PAUSE_GATE";

fn body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn appended_path(output: &Path, suffix: &str) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn sidecar_path(output: &Path) -> PathBuf {
    appended_path(output, ".bytehaul")
}

fn lock_path(output: &Path) -> PathBuf {
    appended_path(output, ".rustypac.lock")
}

fn has_valid_resume_state(output: &Path) -> bool {
    let output_has_data = fs::metadata(output).is_ok_and(|metadata| metadata.len() > 0);
    let control = fs::read(sidecar_path(output)).unwrap_or_default();
    output_has_data && control.len() > 16 && control.starts_with(&0x4259_4845_u32.to_le_bytes())
}

fn range_start(request: &RecordedRequest) -> Option<u64> {
    request
        .header("range")?
        .strip_prefix("bytes=")?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

fn send_signal(pid: u32, signal: libc::c_int) {
    // SAFETY: kill is called with a currently owned child PID and a valid signal number.
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(
        result,
        0,
        "failed to send signal {signal}: {}",
        io::Error::last_os_error()
    );
}

fn process_is_stopped(pid: u32) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("State:"))
        .is_some_and(|line| line.split_whitespace().nth(1) == Some("T"))
}

fn wait_until(mut predicate: impl FnMut() -> bool, description: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn open_pty(width: u16) -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: openpty initializes both descriptors on success; all pointers remain valid.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty failed: {}", io::Error::last_os_error());

    // SAFETY: fcntl reads and updates flags on the valid master descriptor.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL failed: {}", io::Error::last_os_error());
    // SAFETY: the descriptor and flags are valid; O_NONBLOCK only affects reads in this test.
    assert_eq!(
        unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "F_SETFL failed: {}",
        io::Error::last_os_error()
    );

    // SAFETY: each newly opened descriptor is transferred to exactly one File owner.
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

struct ChildResult {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

struct ChildGuard {
    child: Option<Child>,
    master: File,
    stdout: Vec<u8>,
}

impl ChildGuard {
    fn spawn(url: &str, output: &Path, width: u16) -> Self {
        Self::spawn_inner(url, output, width, None)
    }

    fn spawn_with_pause_gate(url: &str, output: &Path, width: u16, gate: &Path) -> Self {
        Self::spawn_inner(url, output, width, Some(gate))
    }

    fn spawn_inner(url: &str, output: &Path, width: u16, gate: Option<&Path>) -> Self {
        let (master, slave) = open_pty(width);
        let mut command = Command::new(env!("CARGO_BIN_EXE_RustyPac"));
        command
            .arg(url)
            .arg(output)
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped());
        if let Some(gate) = gate {
            command.env(PAUSE_GATE_ENV, gate);
        }
        let child = command.spawn().expect("spawn RustyPac child");
        Self {
            child: Some(child),
            master,
            stdout: Vec::new(),
        }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("live child").id()
    }

    fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .expect("live child")
            .try_wait()
            .expect("poll child")
            .is_none()
    }

    fn drain_stdout(&mut self) {
        let mut buffer = [0_u8; 4096];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.stdout.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("read child PTY: {error}"),
            }
        }
    }

    fn captured(&mut self) -> &[u8] {
        self.drain_stdout();
        &self.stdout
    }

    fn set_width(&self, width: u16) {
        let size = libc::winsize {
            ws_row: 24,
            ws_col: width,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master is a valid PTY and size points to a valid winsize value.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0,
            "TIOCSWINSZ failed: {}",
            io::Error::last_os_error()
        );
    }

    fn wait(mut self) -> ChildResult {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let observed_status = loop {
            self.drain_stdout();
            if let Some(status) = self
                .child
                .as_mut()
                .expect("live child")
                .try_wait()
                .expect("poll child")
            {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "child did not exit before deadline"
            );
            thread::sleep(POLL_INTERVAL);
        };
        self.drain_stdout();
        let mut child = self.child.take().expect("live child");
        let status = child.wait().expect("reap exited child");
        assert_eq!(
            status, observed_status,
            "child exit status changed after reap"
        );
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("captured stderr")
            .read_to_string(&mut stderr)
            .expect("read child stderr");
        ChildResult {
            status,
            stdout: std::mem::take(&mut self.stdout),
            stderr,
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

#[test]
fn catchable_signals_checkpoint_finish_once_unlock_and_exit_nonzero() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let expected = body(16 * 1024 * 1024 + 123);
        let server = HttpServer::spawn(
            ServerConfig::new("/catchable", expected.clone(), ResponseMode::Ranges)
                .throttled(8 * 1024, Duration::from_millis(4)),
        );
        let directory = tempdir().unwrap();
        let output = directory.path().join(format!("signal-{signal}.pkg.part"));
        let mut child = ChildGuard::spawn(&server.url("/catchable"), &output, 160);

        wait_until(
            || {
                child.drain_stdout();
                child.is_running()
                    && fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0)
                    && lock_path(&output).exists()
                    && server.bytes_sent() > 2 * 1024 * 1024
            },
            "durable download progress",
            Duration::from_secs(5),
        );
        send_signal(child.id(), signal);
        let result = child.wait();

        assert_eq!(result.status.code(), Some(1), "stderr: {}", result.stderr);
        assert_eq!(
            result.status.signal(),
            None,
            "signal must be handled cooperatively"
        );
        let interrupted_output_len = fs::metadata(&output)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let interrupted_control = fs::read(sidecar_path(&output)).unwrap_or_default();
        assert!(
            has_valid_resume_state(&output),
            "signal {signal} lost resume state: output={interrupted_output_len}, control={}, bytes_sent={}, requests={}",
            interrupted_control.len(),
            server.bytes_sent(),
            server.requests().len(),
        );
        assert!(
            !lock_path(&output).exists(),
            "signal {signal} left a live lock"
        );
        assert!(result.stdout.windows(RED.len()).any(|bytes| bytes == RED));
        assert_eq!(
            result.stdout.iter().filter(|&&byte| byte == b'\n').count(),
            1,
            "signal {signal} must finish exactly one interrupted terminal row"
        );

        drop(server);
        let recovery_server = HttpServer::spawn(ServerConfig::new(
            "/catchable",
            expected.clone(),
            ResponseMode::Ranges,
        ));
        let recovered = ChildGuard::spawn(&recovery_server.url("/catchable"), &output, 160).wait();
        assert!(recovered.status.success(), "stderr: {}", recovered.stderr);
        assert_eq!(fs::read(&output).unwrap(), expected);
        assert!(!sidecar_path(&output).exists());
        assert!(!lock_path(&output).exists());
        assert!(
            recovery_server
                .requests()
                .iter()
                .filter_map(range_start)
                .any(|start| start > 0),
            "signal {signal} checkpoint must resume from a nonzero range"
        );
    }
}

fn wait_for_checkpoint_gate(child: &mut ChildGuard, gate: &Path) {
    let ready = appended_path(gate, ".ready");
    let pid = child.id();
    wait_until(
        || ready.exists() || process_is_stopped(pid) || !child.is_running(),
        "checkpoint gate",
        Duration::from_secs(5),
    );
    assert!(
        ready.exists(),
        "child stopped or exited before exposing the checkpoint control window"
    );
}

fn release_checkpoint_after_queued_signal(gate: &Path, queued_suffix: &str) {
    let queued = appended_path(gate, queued_suffix);
    wait_until(
        || queued.exists(),
        "queued signal acknowledgement",
        Duration::from_secs(2),
    );
    fs::write(appended_path(gate, ".release"), b"release").unwrap();
}

#[test]
fn interrupt_queued_during_suspend_checkpoint_exits_without_sigstop() {
    let expected = body(16 * 1024 * 1024 + 411);
    let server = HttpServer::spawn(
        ServerConfig::new("/checkpoint-interrupt", expected, ResponseMode::Ranges)
            .throttled(8 * 1024, Duration::from_millis(4)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("checkpoint-interrupt.pkg.part");
    let gate = directory.path().join("interrupt-gate");
    let mut child = ChildGuard::spawn_with_pause_gate(
        &server.url("/checkpoint-interrupt"),
        &output,
        160,
        &gate,
    );

    wait_until(
        || {
            child.is_running()
                && fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0)
                && server.bytes_sent() > 2 * 1024 * 1024
        },
        "durable download progress",
        Duration::from_secs(5),
    );
    send_signal(child.id(), libc::SIGTSTP);
    wait_for_checkpoint_gate(&mut child, &gate);
    send_signal(child.id(), libc::SIGINT);
    release_checkpoint_after_queued_signal(&gate, ".interrupt.queued");

    let pid = child.id();
    wait_until(
        || !child.is_running() || process_is_stopped(pid),
        "post-checkpoint interrupt transition",
        Duration::from_secs(5),
    );
    assert!(
        !process_is_stopped(pid),
        "queued interrupt was stranded behind SIGSTOP"
    );
    let result = child.wait();
    assert_eq!(result.status.code(), Some(1), "stderr: {}", result.stderr);
    assert!(has_valid_resume_state(&output));
    assert!(!lock_path(&output).exists());
    assert!(result.stdout.windows(RED.len()).any(|bytes| bytes == RED));
    assert_eq!(
        result.stdout.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "queued interrupt must finish only the interrupted row"
    );
}

#[test]
fn continue_queued_during_suspend_checkpoint_skips_sigstop_and_resumes() {
    let expected = body(16 * 1024 * 1024 + 433);
    let server = HttpServer::spawn(
        ServerConfig::new(
            "/checkpoint-continue",
            expected.clone(),
            ResponseMode::Ranges,
        )
        .throttled(8 * 1024, Duration::from_millis(4)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("checkpoint-continue.pkg.part");
    let gate = directory.path().join("continue-gate");
    let mut child =
        ChildGuard::spawn_with_pause_gate(&server.url("/checkpoint-continue"), &output, 160, &gate);

    wait_until(
        || {
            child.is_running()
                && fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0)
                && server.bytes_sent() > 2 * 1024 * 1024
        },
        "durable download progress",
        Duration::from_secs(5),
    );
    let requests_before_pause = server.requests().len();
    send_signal(child.id(), libc::SIGTSTP);
    wait_for_checkpoint_gate(&mut child, &gate);
    send_signal(child.id(), libc::SIGCONT);
    release_checkpoint_after_queued_signal(&gate, ".continue.queued");

    let pid = child.id();
    wait_until(
        || process_is_stopped(pid) || !child.is_running(),
        "post-checkpoint stop or completion",
        Duration::from_secs(5),
    );
    assert!(
        !process_is_stopped(pid),
        "pre-stop SIGCONT was consumed before an unconditional SIGSTOP"
    );
    let result = child.wait();
    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    assert!(!lock_path(&output).exists());
    assert_eq!(
        result.stdout.iter().filter(|&&byte| byte == b'\n').count(),
        1,
        "skipped stop must not finalize a paused row"
    );
    assert!(
        server.requests()[requests_before_pause..]
            .iter()
            .filter_map(range_start)
            .any(|start| start > 0),
        "pre-stop CONT must recreate a nonzero resumed transfer"
    );
}

#[test]
fn tstp_checkpoints_and_quiesces_before_stop_then_cont_recreates_transfer() {
    let expected = body(16 * 1024 * 1024 + 321);
    let server = HttpServer::spawn(
        ServerConfig::new("/suspend", expected.clone(), ResponseMode::Ranges)
            .throttled(8 * 1024, Duration::from_millis(4)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("suspend.pkg.part");
    let mut child = ChildGuard::spawn(&server.url("/suspend"), &output, 160);

    wait_until(
        || {
            child.is_running()
                && fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0)
                && server.bytes_sent() > 2 * 1024 * 1024
        },
        "durable download progress",
        Duration::from_secs(5),
    );
    send_signal(child.id(), libc::SIGTSTP);
    wait_until(
        || process_is_stopped(child.id()),
        "cooperative process stop",
        Duration::from_secs(5),
    );

    let stopped_output_len = fs::metadata(&output)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let stopped_control = fs::read(sidecar_path(&output)).unwrap_or_default();
    assert!(
        has_valid_resume_state(&output),
        "process stopped before checkpointing: output={stopped_output_len}, control={}, bytes_sent={}, requests={}",
        stopped_control.len(),
        server.bytes_sent(),
        server.requests().len(),
    );
    thread::sleep(Duration::from_millis(100));
    let stopped_bytes = server.bytes_sent();
    let requests_before_continue = server.requests().len();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        server.bytes_sent(),
        stopped_bytes,
        "server traffic continued while RustyPac was stopped"
    );

    send_signal(child.id(), libc::SIGCONT);
    wait_until(
        || !process_is_stopped(child.id()),
        "SIGCONT resume",
        Duration::from_secs(2),
    );
    wait_until(
        || {
            server.requests()[requests_before_continue..]
                .iter()
                .filter_map(range_start)
                .any(|start| start > 0)
        },
        "recreated nonzero range transfer",
        Duration::from_secs(5),
    );
    let result = child.wait();

    assert!(result.status.success(), "stderr: {}", result.stderr);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    assert!(!lock_path(&output).exists());
    assert_eq!(
        result.stdout.iter().filter(|&&byte| byte == b'\n').count(),
        2,
        "suspend and completion must each finalize one row"
    );
}

#[test]
fn sigkill_leaves_stale_state_that_next_invocation_recovers() {
    let expected = body(64 * 1024 * 1024 + 777);
    let slow_server = HttpServer::spawn(
        ServerConfig::new("/killed", expected.clone(), ResponseMode::Ranges)
            .throttled(4 * 1024, Duration::from_millis(4)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("killed.pkg.part");
    let mut killed = ChildGuard::spawn(&slow_server.url("/killed"), &output, 160);

    wait_until(
        || killed.is_running() && lock_path(&output).exists() && has_valid_resume_state(&output),
        "durable state before SIGKILL",
        Duration::from_secs(14),
    );
    send_signal(killed.id(), libc::SIGKILL);
    let killed_result = killed.wait();
    assert_eq!(killed_result.status.signal(), Some(libc::SIGKILL));
    assert!(
        lock_path(&output).exists(),
        "SIGKILL unexpectedly cleaned the lock"
    );
    assert!(has_valid_resume_state(&output));
    drop(slow_server);

    let recovery_server = HttpServer::spawn(ServerConfig::new(
        "/killed",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let recovered = ChildGuard::spawn(&recovery_server.url("/killed"), &output, 160).wait();

    assert!(recovered.status.success(), "stderr: {}", recovered.stderr);
    assert_eq!(fs::read(&output).unwrap(), expected);
    assert!(!sidecar_path(&output).exists());
    assert!(!lock_path(&output).exists());
    assert!(
        recovery_server
            .requests()
            .iter()
            .filter_map(range_start)
            .any(|start| start > 0),
        "stale-state recovery must resume from a nonzero range"
    );
}

#[test]
fn winch_forces_immediate_current_width_redraw() {
    let expected = body(16 * 1024 * 1024 + 999);
    let server = HttpServer::spawn(
        ServerConfig::new("/resize", expected, ResponseMode::Ranges)
            .throttled(8 * 1024, Duration::from_millis(5)),
    );
    let directory = tempdir().unwrap();
    let output = directory.path().join("resize.pkg.part");
    let mut child = ChildGuard::spawn(&server.url("/resize"), &output, 160);

    wait_until(
        || {
            child
                .captured()
                .iter()
                .filter(|&&byte| byte == b'\r')
                .count()
                >= 1
                && !server.requests().is_empty()
        },
        "initial terminal frame",
        Duration::from_secs(5),
    );
    let initial_frames = child
        .captured()
        .iter()
        .filter(|&&byte| byte == b'\r')
        .count();
    child.set_width(60);
    let redraw_started = Instant::now();
    send_signal(child.id(), libc::SIGWINCH);
    wait_until(
        || {
            child
                .captured()
                .iter()
                .filter(|&&byte| byte == b'\r')
                .count()
                > initial_frames
        },
        "forced resize frame",
        Duration::from_millis(750),
    );
    assert!(redraw_started.elapsed() < Duration::from_secs(1));

    send_signal(child.id(), libc::SIGINT);
    let result = child.wait();
    assert_eq!(result.status.code(), Some(1), "stderr: {}", result.stderr);
    assert!(!lock_path(&output).exists());
}
