use unicode_width::UnicodeWidthStr;

use crate::progress::{DisplayState, ProgressModel};

const MIN_FILENAME_WIDTH: usize = 23;
const RESET: &str = "\x1b[0m";
const PURPLE: &str = "\x1b[35m";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutMode {
    Full,
    Small,
    Minimal,
    Compact,
    Plain,
    Extreme,
}

pub fn layout_mode(model: &ProgressModel, width: usize) -> LayoutMode {
    let fields = Fields::from(model);
    for mode in [
        LayoutMode::Full,
        LayoutMode::Small,
        LayoutMode::Minimal,
        LayoutMode::Compact,
    ] {
        if width >= structured_minimum_width(model, &fields, mode) {
            return mode;
        }
    }

    if width >= plain_minimum_width(model, &fields) {
        LayoutMode::Plain
    } else {
        LayoutMode::Extreme
    }
}

pub fn format_row(model: &ProgressModel, width: usize, color: bool) -> String {
    let fields = Fields::from(model);
    let mode = layout_mode(model, width);
    let row = match mode {
        LayoutMode::Full | LayoutMode::Small | LayoutMode::Minimal | LayoutMode::Compact => {
            let minimum = structured_minimum_width(model, &fields, mode);
            structured_row(model, &fields, mode, MIN_FILENAME_WIDTH + width - minimum)
        }
        LayoutMode::Plain => plain_row(model, &fields, width),
        LayoutMode::Extreme => {
            let plain = plain_row(model, &fields, plain_minimum_width(model, &fields));
            truncate_end(&plain, width)
        }
    };

    if color {
        colorize(&row, model.state, mode)
    } else {
        row
    }
}

struct Fields {
    percentage: String,
    bar: String,
    current: String,
    transfer: String,
    speed: String,
    time: String,
    full_time: String,
}

impl Fields {
    fn from(model: &ProgressModel) -> Self {
        if model.state == DisplayState::Skipped {
            return Self {
                percentage: "N/A".to_owned(),
                bar: "N/A".to_owned(),
                current: "N/A".to_owned(),
                transfer: "N/A".to_owned(),
                speed: "N/A".to_owned(),
                time: "N/A".to_owned(),
                full_time: "N/A".to_owned(),
            };
        }

        let percentage = percentage(model);
        let current = format_bytes(model.downloaded);
        let transfer = match model.total {
            Some(total) => format_transfer(model.downloaded, total),
            None => "N/A".to_owned(),
        };
        let speed = model
            .bytes_per_second
            .map(format_speed)
            .unwrap_or_else(|| "N/A".to_owned());
        let time = if model.state == DisplayState::Success {
            format_duration(model.elapsed)
        } else {
            model
                .eta
                .map(format_duration)
                .unwrap_or_else(|| "N/A".to_owned())
        };
        let full_time = if model.state == DisplayState::Success {
            format!("DONE {time}")
        } else if time == "N/A" {
            time.clone()
        } else {
            format!("ETA  {time}")
        };

        Self {
            bar: progress_bar(model),
            percentage,
            current,
            transfer,
            speed,
            time,
            full_time,
        }
    }
}

fn structured_minimum_width(model: &ProgressModel, fields: &Fields, mode: LayoutMode) -> usize {
    UnicodeWidthStr::width(structured_row(model, fields, mode, MIN_FILENAME_WIDTH).as_str())
}

fn plain_minimum_width(model: &ProgressModel, fields: &Fields) -> usize {
    let filename_width = UnicodeWidthStr::width(model.filename.as_str()).min(MIN_FILENAME_WIDTH);
    UnicodeWidthStr::width(plain_row_with_filename_width(model, fields, filename_width).as_str())
}

fn structured_row(
    model: &ProgressModel,
    fields: &Fields,
    mode: LayoutMode,
    filename_width: usize,
) -> String {
    let prefix = padded_prefix(model, filename_width);
    match mode {
        LayoutMode::Full => {
            if model.state == DisplayState::Skipped {
                format!(
                    "{prefix}  ▐  {:>24}   ▐       N/A           ▐     N/A      ▐  N/A",
                    fields.bar
                )
            } else {
                format!(
                    "{prefix}  ▐  {} {:>4}  ▐  {:^17}  ▐  {:>10}  ▐  {}",
                    fields.bar, fields.percentage, fields.transfer, fields.speed, fields.full_time
                )
            }
        }
        LayoutMode::Small => format!(
            "{prefix}  ▐  {:>4}  ▐  {:>9}  ▐  {:>10}  ▐ {}",
            fields.percentage, fields.current, fields.speed, fields.time
        ),
        LayoutMode::Minimal => format!(
            "{prefix}  ▐  {:>4}  ▐  {:>10}  ▐ {}",
            fields.percentage, fields.speed, fields.time
        ),
        LayoutMode::Compact => format!(
            "{prefix}  {:>4}  {:>10} {}",
            fields.percentage, fields.speed, fields.time
        ),
        LayoutMode::Plain | LayoutMode::Extreme => unreachable!("not a structured layout"),
    }
}

fn plain_row(model: &ProgressModel, fields: &Fields, width: usize) -> String {
    let fixed_width = UnicodeWidthStr::width(plain_suffix(fields).as_str()) + icon_width(model) + 1;
    let filename_width = width.saturating_sub(fixed_width);
    plain_row_with_filename_width(model, fields, filename_width)
}

fn plain_row_with_filename_width(
    model: &ProgressModel,
    fields: &Fields,
    filename_width: usize,
) -> String {
    let filename = truncate_end(&model.filename, filename_width);
    format!("{} {filename}{}", icon(model.state), plain_suffix(fields))
}

fn plain_suffix(fields: &Fields) -> String {
    if fields.percentage == "N/A" {
        " N/A".to_owned()
    } else {
        format!(" {} {} {}", fields.percentage, fields.speed, fields.time)
    }
}

fn padded_prefix(model: &ProgressModel, filename_width: usize) -> String {
    let filename = truncate_end(&model.filename, filename_width);
    let padding = filename_width.saturating_sub(UnicodeWidthStr::width(filename.as_str()));
    format!("{} {filename}{}", icon(model.state), " ".repeat(padding))
}

fn icon(state: DisplayState) -> &'static str {
    if state == DisplayState::Skipped {
        "⛓️‍💥"
    } else {
        "📦"
    }
}

fn icon_width(model: &ProgressModel) -> usize {
    UnicodeWidthStr::width(icon(model.state))
}

fn percentage(model: &ProgressModel) -> String {
    match model.total {
        Some(0) | None => "N/A".to_owned(),
        Some(total) => {
            let percent = ((u128::from(model.downloaded) * 100) / u128::from(total)).min(100);
            format!("{percent}%")
        }
    }
}

fn progress_bar(model: &ProgressModel) -> String {
    let filled = match model.total {
        Some(0) | None => 0,
        Some(total) => ((u128::from(model.downloaded) * 20) / u128::from(total)).min(20) as usize,
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(20 - filled))
}

fn format_transfer(downloaded: u64, total: u64) -> String {
    let (divisor, unit) = byte_unit(total.max(downloaded));
    format!(
        "{:.1} / {:.1} {unit}",
        downloaded as f64 / divisor,
        total as f64 / divisor
    )
}

fn format_bytes(bytes: u64) -> String {
    let (divisor, unit) = byte_unit(bytes);
    if divisor == 1.0 {
        format!("{bytes} {unit}")
    } else {
        format!("{:.1} {unit}", bytes as f64 / divisor)
    }
}

fn format_speed(bytes_per_second: u64) -> String {
    let (divisor, unit) = byte_unit(bytes_per_second);
    if divisor == 1.0 {
        format!("{bytes_per_second}{unit}/s")
    } else {
        format!("{:.1}{unit}/s", bytes_per_second as f64 / divisor)
    }
}

fn byte_unit(bytes: u64) -> (f64, &'static str) {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        (GIB as f64, "GiB")
    } else if bytes >= MIB {
        (MIB as f64, "MiB")
    } else if bytes >= KIB {
        (KIB as f64, "KiB")
    } else {
        (1.0, "B")
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn truncate_end(text: &str, target_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= target_width {
        return text.to_owned();
    }
    if target_width <= 3 {
        return ".".repeat(target_width);
    }

    let content_width = target_width - 3;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let candidate_end = index + character.len_utf8();
        if UnicodeWidthStr::width(&text[..candidate_end]) > content_width {
            break;
        }
        end = candidate_end;
    }
    let prefix = &text[..end];
    let padding = content_width - UnicodeWidthStr::width(prefix);
    format!("{prefix}{}...", " ".repeat(padding))
}

fn colorize(row: &str, state: DisplayState, mode: LayoutMode) -> String {
    let content_color = match state {
        DisplayState::Active | DisplayState::Paused => "\x1b[37m",
        DisplayState::Success => "\x1b[32m",
        DisplayState::Skipped => "\x1b[90m",
        DisplayState::Error | DisplayState::Interrupted => "\x1b[31m",
    };
    if matches!(
        mode,
        LayoutMode::Full | LayoutMode::Small | LayoutMode::Minimal
    ) {
        format!(
            "{content_color}{}{RESET}",
            row.replace('▐', &format!("{RESET}{PURPLE}▐{RESET}{content_color}"))
        )
    } else {
        format!("{content_color}{row}{RESET}")
    }
}
