#[allow(dead_code)]
#[path = "../src/progress.rs"]
mod progress;
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::rc::Rc;
use std::time::Duration;

use progress::{DisplayState, ProgressModel};
use render::{Clock, Renderer, TerminalInfo, TerminalProbe};
use terminal_size::{terminal_size_of, Width};
use unicode_width::UnicodeWidthStr;

const FULL: &str = "📦 very_long_filename_t...  ▐  ██████████░░░░░░░░░░  50%  ▐    1.0 / 2.0 GiB    ▐   10.3MiB/s  ▐  ETA  00:30";
const SMALL: &str = "📦 very_long_filename_t...  ▐   50%  ▐    1.0 GiB  ▐   10.3MiB/s  ▐ 00:30";

fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn active_model(filename: &str) -> ProgressModel {
    ProgressModel {
        filename: filename.to_owned(),
        state: DisplayState::Active,
        downloaded: 1 << 30,
        total: Some(2 << 30),
        bytes_per_second: Some(10_800_333),
        elapsed: Duration::from_secs(90),
        eta: Some(Duration::from_secs(30)),
    }
}

fn completed_model(filename: &str) -> ProgressModel {
    ProgressModel {
        filename: filename.to_owned(),
        state: DisplayState::Success,
        downloaded: 2 << 30,
        total: Some(2 << 30),
        bytes_per_second: Some(10_800_333),
        elapsed: Duration::from_secs(120),
        eta: None,
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    bytes: Rc<RefCell<Vec<u8>>>,
    flushes: Rc<Cell<usize>>,
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }

    fn flushes(&self) -> usize {
        self.flushes.get()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes.borrow_mut().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes.set(self.flushes.get() + 1);
        Ok(())
    }
}

#[derive(Clone)]
struct FakeClock(Rc<Cell<Duration>>);

impl FakeClock {
    fn new() -> Self {
        Self(Rc::new(Cell::new(Duration::ZERO)))
    }

    fn set(&self, now: Duration) {
        self.0.set(now);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

#[derive(Clone, Copy)]
struct FixedProbe(TerminalInfo);

impl TerminalProbe for FixedProbe {
    fn probe(&self) -> TerminalInfo {
        self.0
    }
}

#[test]
fn changed_frames_are_throttled_and_forced_frames_bypass_timing() {
    let writer = SharedWriter::default();
    let captured = writer.clone();
    let clock = FakeClock::new();
    let mut renderer = Renderer::new(
        writer,
        FixedProbe(TerminalInfo::terminal(width(FULL))),
        clock.clone(),
    );

    renderer.update(&active_model("first.pkg"), false).unwrap();
    let after_first = captured.bytes();
    assert!(after_first.starts_with(b"\r"));
    assert!(after_first.ends_with(b"\x1b[K"));
    assert!(after_first.windows(9).any(|part| part == b"first.pkg"));
    assert_eq!(captured.flushes(), 1);

    clock.set(Duration::from_millis(999));
    renderer
        .update(&active_model("too-early.pkg"), false)
        .unwrap();
    assert_eq!(captured.bytes(), after_first);

    clock.set(Duration::from_secs(1));
    renderer
        .update(&active_model("one-second.pkg"), false)
        .unwrap();
    let after_one_second = captured.bytes();
    let second_frame = &after_one_second[after_first.len()..];
    assert!(second_frame.starts_with(b"\r"));
    assert!(second_frame.ends_with(b"\x1b[K"));
    assert!(second_frame
        .windows(14)
        .any(|part| part == b"one-second.pkg"));
    assert!(!second_frame.starts_with(b"\r\x1b[K"));
    assert_eq!(captured.flushes(), 2);

    clock.set(Duration::from_secs(2));
    renderer
        .update(&active_model("one-second.pkg"), false)
        .unwrap();
    assert_eq!(captured.bytes(), after_one_second);

    clock.set(Duration::from_millis(2_100));
    renderer.update(&active_model("forced.pkg"), true).unwrap();
    let after_force = captured.bytes();
    assert!(after_force[after_one_second.len()..]
        .windows(10)
        .any(|part| part == b"forced.pkg"));
    assert_eq!(captured.flushes(), 3);

    renderer.update(&active_model("forced.pkg"), true).unwrap();
    assert_eq!(captured.bytes(), after_force);
    assert_eq!(captured.flushes(), 3);
}

#[derive(Clone)]
struct MutableProbe {
    terminal: Rc<Cell<TerminalInfo>>,
    calls: Rc<Cell<usize>>,
}

impl MutableProbe {
    fn new(terminal: TerminalInfo) -> Self {
        Self {
            terminal: Rc::new(Cell::new(terminal)),
            calls: Rc::new(Cell::new(0)),
        }
    }

    fn set(&self, terminal: TerminalInfo) {
        self.terminal.set(terminal);
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }
}

impl TerminalProbe for MutableProbe {
    fn probe(&self) -> TerminalInfo {
        self.calls.set(self.calls.get() + 1);
        self.terminal.get()
    }
}

fn strip_ansi(text: &str) -> String {
    let mut result = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            for next in characters.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else {
            result.push(character);
        }
    }
    result
}

#[test]
fn forced_resize_reprobes_width_and_redraws_immediately() {
    let writer = SharedWriter::default();
    let captured = writer.clone();
    let probe = MutableProbe::new(TerminalInfo::terminal(width(FULL)));
    let mut renderer = Renderer::new(writer, probe.clone(), FakeClock::new());
    let model = active_model("very_long_filename_that_does_not_fit.zst");

    renderer.update(&model, false).unwrap();
    let first_frame = captured.bytes();
    assert_eq!(
        strip_ansi(std::str::from_utf8(&first_frame).unwrap()),
        format!("\r{FULL}")
    );
    assert_eq!(probe.calls(), 1);

    probe.set(TerminalInfo::terminal(width(SMALL)));
    renderer.update(&model, true).unwrap();
    let all_frames = captured.bytes();
    let resized_frame = std::str::from_utf8(&all_frames[first_frame.len()..]).unwrap();

    assert_eq!(strip_ansi(resized_frame), format!("\r{SMALL}"));
    assert_eq!(probe.calls(), 2);
}

struct FileProbe {
    file: File,
    redirected_width: usize,
}

impl TerminalProbe for FileProbe {
    fn probe(&self) -> TerminalInfo {
        match terminal_size_of(&self.file) {
            Some((Width(width), _)) => TerminalInfo::terminal(usize::from(width)),
            None => TerminalInfo::redirected(self.redirected_width),
        }
    }
}

#[cfg(unix)]
fn open_pty(width: u16) -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: openpty initializes both file descriptors on success; the winsize
    // is valid for the duration of the call and no name/termios output is needed.
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

    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: slave is a valid PTY descriptor and attributes points to writable storage.
    assert_eq!(
        unsafe { libc::tcgetattr(slave, attributes.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr initialized attributes after the successful call above.
    let mut attributes = unsafe { attributes.assume_init() };
    // SAFETY: attributes is an initialized termios structure.
    unsafe { libc::cfmakeraw(&mut attributes) };
    // SAFETY: slave is valid and attributes remains initialized.
    assert_eq!(
        unsafe { libc::tcsetattr(slave, libc::TCSANOW, &attributes) },
        0
    );

    // SAFETY: ownership of each newly opened descriptor transfers to exactly one File.
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

#[cfg(unix)]
fn read_closed_pty(mut master: File) -> Vec<u8> {
    let mut bytes = Vec::new();
    match master.read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
        Err(error) => panic!("failed reading PTY: {error}"),
    }
    bytes
}

#[cfg(unix)]
#[test]
fn pty_uses_in_place_ansi_while_redirected_output_uses_flushed_records() {
    let clock = FakeClock::new();
    let (master, slave) = open_pty(width(FULL) as u16);
    let probe_file = slave.try_clone().unwrap();
    let mut terminal_renderer = Renderer::new(
        slave,
        FileProbe {
            file: probe_file,
            redirected_width: width(FULL),
        },
        clock.clone(),
    );

    terminal_renderer
        .update(&active_model("terminal.pkg"), false)
        .unwrap();
    terminal_renderer
        .finish(&completed_model("terminal.pkg"))
        .unwrap();
    terminal_renderer
        .finish(&completed_model("terminal.pkg"))
        .unwrap();
    drop(terminal_renderer);

    let terminal_output = read_closed_pty(master);
    assert!(terminal_output.contains(&b'\r'));
    assert!(terminal_output.windows(2).any(|part| part == b"\x1b["));
    assert!(terminal_output.windows(3).any(|part| part == b"\x1b[K"));
    assert!(!terminal_output.windows(4).any(|part| part == b"\r\x1b[K"));
    assert_eq!(
        terminal_output
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count(),
        1
    );
    assert!(terminal_output.ends_with(b"\n"));

    let redirected_fd = tempfile::tempfile().unwrap();
    let redirected_writer = SharedWriter::default();
    let captured = redirected_writer.clone();
    let mut redirected_renderer = Renderer::new(
        redirected_writer,
        FileProbe {
            file: redirected_fd,
            redirected_width: width(FULL),
        },
        clock,
    );

    redirected_renderer
        .update(&active_model("redirected.pkg"), false)
        .unwrap();
    let before_finish = captured.bytes();
    assert_eq!(
        before_finish.iter().filter(|&&byte| byte == b'\n').count(),
        1
    );
    assert_eq!(captured.flushes(), 1);

    redirected_renderer
        .finish(&completed_model("redirected.pkg"))
        .unwrap();
    redirected_renderer
        .finish(&completed_model("redirected.pkg"))
        .unwrap();
    let redirected_output = captured.bytes();
    assert!(!redirected_output.contains(&b'\r'));
    assert!(!redirected_output.contains(&0x1b));
    assert_eq!(
        redirected_output
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count(),
        2
    );
    assert!(redirected_output.ends_with(b"\n"));
    assert_eq!(captured.flushes(), 2);
}
