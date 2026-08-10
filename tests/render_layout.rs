#[path = "../src/progress.rs"]
mod progress;
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;

use std::time::Duration;

use progress::{DisplayState, ProgressModel};
use render::{format_row, layout_mode, LayoutMode};
use unicode_width::UnicodeWidthStr;

const FULL: &str = "📦 very_long_filename_t...  ▐  ██████████░░░░░░░░░░  50%  ▐    1.0 / 2.0 GiB    ▐   10.3MiB/s  ▐  ETA  00:30";
const SMALL: &str = "📦 very_long_filename_t...  ▐   50%  ▐    1.0 GiB  ▐   10.3MiB/s  ▐ 00:30";
const MINIMAL: &str = "📦 very_long_filename_t...  ▐   50%  ▐   10.3MiB/s  ▐ 00:30";
const COMPACT: &str = "📦 very_long_filename_t...   50%   10.3MiB/s 00:30";
const PLAIN: &str = "📦 very_long_filename_t... 50% 10.3MiB/s 00:30";

fn active_model() -> ProgressModel {
    ProgressModel {
        filename: "very_long_filename_that_does_not_fit.zst".to_owned(),
        state: DisplayState::Active,
        downloaded: 1 << 30,
        total: Some(2 << 30),
        bytes_per_second: Some(10_800_333),
        elapsed: Duration::from_secs(90),
        eta: Some(Duration::from_secs(30)),
    }
}

fn completed_model() -> ProgressModel {
    ProgressModel {
        filename: "cachyos-v3.db".to_owned(),
        state: DisplayState::Success,
        downloaded: 128_307,
        total: Some(128_307),
        bytes_per_second: Some(8_493_466),
        elapsed: Duration::ZERO,
        eta: None,
    }
}

fn skipped_model() -> ProgressModel {
    ProgressModel {
        filename: "core.db.sig".to_owned(),
        state: DisplayState::Skipped,
        downloaded: 0,
        total: None,
        bytes_per_second: None,
        elapsed: Duration::ZERO,
        eta: None,
    }
}

fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn ghostty_width(text: &str) -> usize {
    UnicodeWidthStr::width(text) + text.matches("⛓️‍💥").count() * 2
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
fn formats_approved_active_rows_at_mode_minimums() {
    let model = active_model();
    for (mode, expected) in [
        (LayoutMode::Full, FULL),
        (LayoutMode::Small, SMALL),
        (LayoutMode::Minimal, MINIMAL),
        (LayoutMode::Compact, COMPACT),
        (LayoutMode::Plain, PLAIN),
    ] {
        let actual = format_row(&model, width(expected), false);
        assert_eq!(layout_mode(&model, width(expected)), mode);
        assert_eq!(actual, expected);
        assert_eq!(width(&actual), width(expected));
    }
}

#[test]
fn extreme_truncates_the_entire_plain_row_to_display_width() {
    let target_width = width(PLAIN) - 1;
    let expected = "📦 very_long_filename_t... 50% 10.3MiB/s 0...";

    let actual = format_row(&active_model(), target_width, false);

    assert_eq!(
        layout_mode(&active_model(), target_width),
        LayoutMode::Extreme
    );
    assert_eq!(actual, expected);
    assert_eq!(width(&actual), target_width);
}

#[test]
fn formats_approved_completion_rows() {
    let model = completed_model();
    for expected in [
        "📦 cachyos-v3.db            ▐  ████████████████████ 100%  ▐  125.3 / 125.3 KiB  ▐    8.1MiB/s  ▐  DONE 00:00",
        "📦 cachyos-v3.db            ▐  100%  ▐  125.3 KiB  ▐    8.1MiB/s  ▐ 00:00",
        "📦 cachyos-v3.db            ▐  100%  ▐    8.1MiB/s  ▐ 00:00",
        "📦 cachyos-v3.db            100%    8.1MiB/s 00:00",
        "📦 cachyos-v3.db 100% 8.1MiB/s 00:00",
    ] {
        assert_eq!(format_row(&model, width(expected), false), expected);
    }
}

#[test]
fn formats_approved_skipped_rows_with_unavailable_fields() {
    let model = skipped_model();
    for expected in [
        "⛓️‍💥 core.db.sig              ▐                       N/A   ▐       N/A           ▐     N/A      ▐  N/A",
        "⛓️‍💥 core.db.sig N/A",
    ] {
        assert_eq!(format_row(&model, ghostty_width(expected), false), expected);
    }
}

#[test]
fn skipped_full_row_aligns_with_download_rows_in_ghostty() {
    let terminal_width = width(FULL) + 40;
    let completed = format_row(&completed_model(), terminal_width, false);
    let skipped = format_row(&skipped_model(), terminal_width, false);

    assert_eq!(
        ghostty_width(&skipped),
        ghostty_width(&completed),
        "the broken-chain fallback glyphs must not push skipped fields right"
    );
    assert_eq!(ghostty_width(&skipped), terminal_width);
}

#[test]
fn structured_modes_keep_23_filename_cells_and_give_surplus_only_to_filename() {
    let model = active_model();
    for (mode, minimum) in [
        (LayoutMode::Full, FULL),
        (LayoutMode::Small, SMALL),
        (LayoutMode::Minimal, MINIMAL),
        (LayoutMode::Compact, COMPACT),
    ] {
        let minimum_width = width(minimum);
        assert_eq!(layout_mode(&model, minimum_width), mode);
        assert_eq!(
            width(&format_row(&model, minimum_width, false)),
            minimum_width
        );

        let wider = format_row(&model, minimum_width + 2, false);
        let expected = minimum.replacen("very_long_filename_t...", "very_long_filename_tha...", 1);
        assert_eq!(wider, expected);
        assert_eq!(width(&wider), minimum_width + 2);
    }
}

#[test]
fn each_mode_yields_to_the_next_one_cell_below_its_minimum() {
    let model = active_model();
    for (minimum, next) in [
        (FULL, LayoutMode::Small),
        (SMALL, LayoutMode::Minimal),
        (MINIMAL, LayoutMode::Compact),
        (COMPACT, LayoutMode::Plain),
        (PLAIN, LayoutMode::Extreme),
    ] {
        assert_eq!(layout_mode(&model, width(minimum) - 1), next);
    }
}

#[test]
fn truncation_respects_unicode_display_cells() {
    let mut model = active_model();
    model.filename = "資料📦archive-name-that-is-long.zst".to_owned();
    let row = format_row(&model, width(MINIMAL), false);

    assert_eq!(width(&row), width(MINIMAL));
    assert!(row.contains("..."));
    assert!(!row.contains('\u{fffd}'));
}

#[test]
fn colors_content_by_state_and_structured_separators_purple() {
    const WHITE: &str = "\u{1b}[37m";
    const GREEN: &str = "\u{1b}[32m";
    const DARK_GRAY: &str = "\u{1b}[90m";
    const RED: &str = "\u{1b}[31m";
    const PURPLE: &str = "\u{1b}[35m";

    for (state, color) in [
        (DisplayState::Active, WHITE),
        (DisplayState::Paused, WHITE),
        (DisplayState::Success, GREEN),
        (DisplayState::Skipped, DARK_GRAY),
        (DisplayState::Error, RED),
        (DisplayState::Interrupted, RED),
    ] {
        let mut model = active_model();
        model.state = state;
        let colored = format_row(&model, width(FULL), true);
        assert!(
            colored.contains(color),
            "missing content color for {state:?}"
        );
        assert!(
            colored.contains(PURPLE),
            "missing purple separator for {state:?}"
        );
        assert_eq!(strip_ansi(&colored), format_row(&model, width(FULL), false));
    }
}

#[test]
fn color_false_emits_no_ansi_in_any_mode() {
    for target_width in [
        width(FULL),
        width(SMALL),
        width(MINIMAL),
        width(COMPACT),
        width(PLAIN),
        width(PLAIN) - 1,
    ] {
        assert!(!format_row(&active_model(), target_width, false).contains('\u{1b}'));
    }
}
