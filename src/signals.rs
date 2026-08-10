use std::io;

use tokio::signal::unix::{signal, Signal, SignalKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalEvent {
    Interrupt,
    Terminate,
    Hangup,
    Suspend,
    Continue,
    Resize,
}

pub struct SignalEvents {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
    suspend: Signal,
    continue_: Signal,
    resize: Signal,
}

pub fn subscribe() -> io::Result<SignalEvents> {
    Ok(SignalEvents {
        interrupt: signal(SignalKind::interrupt())?,
        terminate: signal(SignalKind::terminate())?,
        hangup: signal(SignalKind::hangup())?,
        suspend: signal(SignalKind::from_raw(libc::SIGTSTP))?,
        continue_: signal(SignalKind::from_raw(libc::SIGCONT))?,
        resize: signal(SignalKind::window_change())?,
    })
}

impl SignalEvents {
    pub async fn recv(&mut self) -> Option<SignalEvent> {
        tokio::select! {
            event = self.interrupt.recv() => event.map(|()| SignalEvent::Interrupt),
            event = self.terminate.recv() => event.map(|()| SignalEvent::Terminate),
            event = self.hangup.recv() => event.map(|()| SignalEvent::Hangup),
            event = self.suspend.recv() => event.map(|()| SignalEvent::Suspend),
            event = self.continue_.recv() => event.map(|()| SignalEvent::Continue),
            event = self.resize.recv() => event.map(|()| SignalEvent::Resize),
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn record_test_delivery(event: SignalEvent) {
    let Some(base) = std::env::var_os("RUSTYPAC_TEST_PAUSE_GATE") else {
        return;
    };
    let mut path = base;
    path.push(match event {
        SignalEvent::Interrupt => ".interrupt.queued",
        SignalEvent::Terminate => ".terminate.queued",
        SignalEvent::Hangup => ".hangup.queued",
        SignalEvent::Suspend => ".suspend.queued",
        SignalEvent::Continue => ".continue.queued",
        SignalEvent::Resize => ".resize.queued",
    });
    let _ = std::fs::write(std::path::PathBuf::from(path), b"queued");
}

#[cfg(not(debug_assertions))]
pub(crate) fn record_test_delivery(_event: SignalEvent) {}
