use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayState {
    Active,
    Success,
    Skipped,
    Error,
    Interrupted,
    Paused,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressModel {
    pub filename: String,
    pub state: DisplayState,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub bytes_per_second: Option<u64>,
    pub elapsed: Duration,
    pub eta: Option<Duration>,
}
