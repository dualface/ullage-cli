//! ASCII tables and aligned key-value blocks for human-readable CLI output.
//!
//! Color is applied only after cell text has been sanitized. Untrusted provider
//! strings never become part of an escape sequence.

use std::ffi::OsStr;
use std::io::IsTerminal as _;

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthStr;

use crate::ColorMode;
use crate::summary::{SummaryRow, SummaryValue};

const RESET: &str = "\x1b[0m";
const HEADER: &str = "\x1b[1;36m";
const WARNING: &str = "\x1b[33m";
const ERROR: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const KEY_VALUE_GAP: usize = 2;
const COLUMN_GAP: usize = 2;
/// Width of the summary progress bar, in cells.
const BAR_WIDTH: usize = 10;
const BAR_FILLED: char = '#';
const BAR_EMPTY: char = '-';
/// Rendered width of `[` + bar + `]`.
const BAR_RENDER_WIDTH: usize = BAR_WIDTH + 2;
/// Remaining quota at or below which the bar turns red, then yellow.
const CRITICAL_REMAINING: f64 = 0.10;
const LOW_REMAINING: f64 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    pub fn from_mode(mode: ColorMode) -> Self {
        Self::resolve(
            mode,
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    }

    pub fn resolve(mode: ColorMode, stdout_is_tty: bool, no_color: Option<&OsStr>) -> Self {
        let enabled = match mode {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => stdout_is_tty && no_color_allows(no_color),
        };
        Self { enabled }
    }

    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self { enabled: false }
    }

    #[cfg(test)]
    fn enabled(self) -> bool {
        self.enabled
    }

    fn wrap(self, style: Style, text: &str) -> String {
        if !self.enabled {
            return text.to_owned();
        }
        let code = match style {
            Style::Plain => return text.to_owned(),
            Style::Header => HEADER,
            Style::Warning => WARNING,
            Style::Error => ERROR,
            Style::Dim => DIM,
        };
        format!("{code}{text}{RESET}")
    }
}

fn no_color_allows(no_color: Option<&OsStr>) -> bool {
    match no_color {
        None => true,
        Some(value) => value.is_empty(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Header,
    Warning,
    Error,
    Dim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    text: String,
    align: Align,
    style: Style,
}

impl Cell {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self {
            text: visible_cell_text(value.as_ref()),
            align: Align::Left,
            style: Style::Plain,
        }
    }

    pub fn numeric(value: impl AsRef<str>) -> Self {
        Self {
            text: visible_cell_text(value.as_ref()),
            align: Align::Right,
            style: Style::Plain,
        }
    }

    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn used(used: f64, limit: Option<f64>) -> Self {
        Self {
            text: visible_cell_text(&used.to_string()),
            align: Align::Right,
            style: used_style(used, limit),
        }
    }

    pub fn limit(limit: Option<f64>) -> Self {
        match limit {
            Some(value) => Self::numeric(value.to_string()),
            None => Self {
                text: visible_cell_text(""),
                align: Align::Right,
                style: Style::Plain,
            },
        }
    }
}

fn visible_cell_text(value: &str) -> String {
    let sanitized = crate::sanitize_cell(value);
    if sanitized.is_empty() {
        "-".into()
    } else {
        sanitized.to_owned()
    }
}

fn used_style(used: f64, limit: Option<f64>) -> Style {
    match limit {
        Some(limit) if limit > 0.0 => {
            let ratio = used / limit;
            if ratio >= 0.90 {
                Style::Error
            } else if ratio >= 0.75 {
                Style::Warning
            } else {
                Style::Plain
            }
        }
        _ => Style::Plain,
    }
}

fn effective_style(cell: &Cell) -> Style {
    if cell.style == Style::Plain && cell.text == "-" {
        Style::Dim
    } else {
        cell.style
    }
}

/// Display columns, treating CJK and fullwidth as 2 and combining marks as 0.
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let extra = width.saturating_sub(display_width(text));
    match align {
        Align::Left => {
            let mut padded = String::from(text);
            padded.push_str(&" ".repeat(extra));
            padded
        }
        Align::Right => {
            let mut padded = " ".repeat(extra);
            padded.push_str(text);
            padded
        }
    }
}

fn horizontal_border(widths: &[usize]) -> String {
    let mut line = String::from("+");
    for width in widths {
        line.push_str(&"-".repeat(width + 2));
        line.push('+');
    }
    line
}

fn format_row(row: &[Cell], widths: &[usize], palette: Palette, header: bool) -> String {
    let mut line = String::from("|");
    for (index, width) in widths.iter().enumerate() {
        let fallback = Cell::new("");
        let cell = row.get(index).unwrap_or(&fallback);
        let align = if header { Align::Left } else { cell.align };
        let padded = pad(&cell.text, *width, align);
        let style = if header {
            Style::Header
        } else {
            effective_style(cell)
        };
        line.push(' ');
        line.push_str(&palette.wrap(style, &padded));
        line.push_str(" |");
    }
    line
}

pub fn render_table(headers: &[&str], rows: &[Vec<Cell>], palette: &Palette) -> String {
    let columns = headers.len();
    let header_cells: Vec<Cell> = headers.iter().map(|header| Cell::new(*header)).collect();
    let mut widths = vec![0; columns];
    for (index, cell) in header_cells.iter().enumerate() {
        widths[index] = display_width(&cell.text);
    }
    for row in rows {
        for (index, cell) in row.iter().take(columns).enumerate() {
            widths[index] = widths[index].max(display_width(&cell.text));
        }
    }

    let mut output = String::new();
    output.push_str(&horizontal_border(&widths));
    output.push('\n');
    output.push_str(&format_row(&header_cells, &widths, *palette, true));
    output.push('\n');
    output.push_str(&horizontal_border(&widths));
    output.push('\n');
    for row in rows {
        output.push_str(&format_row(row, &widths, *palette, false));
        output.push('\n');
    }
    output.push_str(&horizontal_border(&widths));
    output.push('\n');
    output
}

pub fn render_section_header(text: &str, palette: &Palette) -> String {
    let sanitized = crate::sanitize_cell(text);
    let mut output = palette.wrap(Style::Header, &format!("==== {sanitized} ===="));
    output.push('\n');
    output
}

/// Renders one standalone summary line, such as `updated 2m ago`.
///
/// The text is sanitized before it is wrapped, so a caller that interpolates
/// provider data still cannot emit an escape sequence.
pub fn render_line(text: &str, style: Style, palette: &Palette) -> String {
    let mut output = palette.wrap(style, crate::sanitize_cell(text));
    output.push('\n');
    output
}

/// Renders the summary grid: identity, value, reset, progress bar.
///
/// Window and metric collapse into one identity cell (for example `5h`,
/// `5h-5.3`, or `GrokBuild`) so the table does not repeat `usage` or a full
/// model name. The bar is always the last column so every row shares one
/// aligned slot, including rows without a reset time.
pub fn render_summary_rows(rows: &[SummaryRow], now: DateTime<Utc>, palette: &Palette) -> String {
    let cells: Vec<SummaryCells> = rows.iter().map(|row| summary_cells(row, now)).collect();
    let identity_width = max_width(cells.iter().map(|cell| cell.identity.as_str()));
    let verb_width = max_width(cells.iter().map(|cell| cell.verb));
    let suffix_width = max_width(cells.iter().map(|cell| cell.suffix));
    let resets_width = max_width(cells.iter().map(|cell| cell.resets.as_str()));

    let values: Vec<String> = cells
        .iter()
        .map(|cell| {
            // Amounts line up within one reading of the number. Aligning a
            // percentage against a spend range would only push them apart.
            let amount_width = max_width(
                cells
                    .iter()
                    .filter(|other| other.verb == cell.verb)
                    .map(|other| other.amount.as_str()),
            );
            let mut value = pad(cell.verb, verb_width, Align::Left);
            value.push(' ');
            value.push_str(&pad(&cell.amount, amount_width, Align::Right));
            if suffix_width > 0 {
                value.push(' ');
                value.push_str(&pad(cell.suffix, suffix_width, Align::Left));
            }
            value
        })
        .collect();
    let value_width = max_width(values.iter().map(String::as_str));

    let gap = " ".repeat(COLUMN_GAP);
    let mut output = String::new();
    for (cell, value) in cells.iter().zip(&values) {
        let mut columns = vec![
            pad(&cell.identity, identity_width, Align::Left),
            pad(value, value_width, Align::Left),
        ];
        if resets_width > 0 {
            columns.push(pad(&cell.resets, resets_width, Align::Left));
        }
        let mut line = columns.join(&gap);
        // Padding is kept ahead of a bar so bars share one column even when a
        // neighbouring row has no reset time.
        match cell.remaining_ratio {
            Some(ratio) => {
                line.push_str(&gap);
                line.push_str(&palette.wrap(bar_style(ratio), &progress_bar(ratio)));
            }
            None => line = line.trim_end().to_owned(),
        }
        output.push_str(&line);
        output.push('\n');
    }
    output
}

/// The pre-alignment text of one summary row.
struct SummaryCells {
    identity: String,
    verb: &'static str,
    amount: String,
    suffix: &'static str,
    resets: String,
    remaining_ratio: Option<f64>,
}

fn summary_cells(row: &SummaryRow, now: DateTime<Utc>) -> SummaryCells {
    let (verb, amount) = summary_value_text(&row.value);
    SummaryCells {
        identity: compact_identity(
            &visible_cell_text(&row.window),
            &visible_cell_text(&row.metric),
        ),
        verb,
        amount,
        suffix: if row.disabled { "(off)" } else { "" },
        resets: row
            .resets_at
            .map(|resets_at| format!("resets {}", relative_future(resets_at, now)))
            .unwrap_or_default(),
        remaining_ratio: row.remaining_ratio,
    }
}

/// Collapses window + metric into the single label the table prints.
fn compact_identity(window: &str, metric: &str) -> String {
    let metric = metric.trim();
    if metric.eq_ignore_ascii_case("GrokBuild") {
        return "GrokBuild".into();
    }
    if metric.is_empty() || metric == "usage" {
        return window.to_string();
    }
    if let Some(rest) = strip_gpt_prefix(metric) {
        return format!("{window}-{rest}");
    }
    if metric == "Codex" {
        return format!("{window}-{metric}");
    }
    format!("{window}{}{metric}", " ".repeat(COLUMN_GAP))
}

fn strip_gpt_prefix(metric: &str) -> Option<&str> {
    let rest = metric
        .strip_prefix("GPT-")
        .or_else(|| metric.strip_prefix("gpt-"))?;
    (!rest.is_empty()).then_some(rest)
}

fn summary_value_text(value: &SummaryValue) -> (&'static str, String) {
    match value {
        SummaryValue::Remains(percent) => ("remains", format_percent(*percent)),
        SummaryValue::Used(percent) => ("used", format_percent(*percent)),
        SummaryValue::Balance { amount, currency } => {
            ("balance", format_money(*amount, &currency.code))
        }
        SummaryValue::Spent {
            amount,
            limit,
            currency,
        } => (
            "spent",
            format!(
                "{} of {}",
                format_money(*amount, &currency.code),
                format_money(*limit, &currency.code)
            ),
        ),
        SummaryValue::Credits { used, limit } => (
            "credits",
            match limit {
                Some(limit) => format!("{} of {}", format_number(*used), format_number(*limit)),
                None => format_number(*used),
            },
        ),
        SummaryValue::CreditsUnlimited => ("credits", "unlimited".into()),
        SummaryValue::Counted { used, limit } => (
            "used",
            match limit {
                Some(limit) => format!("{} of {}", format_number(*used), format_number(*limit)),
                None => format_number(*used),
            },
        ),
        SummaryValue::Disabled => ("disabled", String::new()),
    }
}

fn format_percent(percent: f64) -> String {
    if percent.is_finite() {
        format!("{}%", percent.round())
    } else {
        "-".into()
    }
}

fn format_number(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "-".into()
    }
}

/// Formats an amount with its currency symbol, falling back to the raw code.
fn format_money(amount: f64, code: &str) -> String {
    if !amount.is_finite() {
        return "-".into();
    }
    let code = crate::sanitize_cell(code);
    match currency_symbol(code) {
        Some(symbol) => format!("{symbol}{amount:.2}"),
        None if code.is_empty() => format!("{amount:.2}"),
        None => format!("{code} {amount:.2}"),
    }
}

fn currency_symbol(code: &str) -> Option<&'static str> {
    match code {
        "USD" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        _ => None,
    }
}

/// Describes how far `moment` is ahead of `now`, e.g. `in 3h57m`.
pub fn relative_future(moment: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (moment - now).num_seconds();
    if seconds <= 0 {
        return "now".into();
    }
    format!("in {}", duration_text(seconds))
}

/// Describes how far `moment` is behind `now`, e.g. `2m ago`.
pub fn relative_past(moment: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - moment).num_seconds().max(0);
    format!("{} ago", duration_text(seconds))
}

fn duration_text(seconds: i64) -> String {
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d{:02}h", hours % 24)
    } else if hours > 0 {
        format!("{hours}h{:02}m", minutes % 60)
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".into()
    }
}

fn progress_bar(remaining_ratio: f64) -> String {
    let filled = (remaining_ratio.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let mut bar = String::with_capacity(BAR_RENDER_WIDTH);
    bar.push('[');
    bar.extend(std::iter::repeat_n(BAR_EMPTY, BAR_WIDTH - filled));
    bar.extend(std::iter::repeat_n(BAR_FILLED, filled));
    bar.push(']');
    bar
}

fn bar_style(remaining_ratio: f64) -> Style {
    if remaining_ratio <= CRITICAL_REMAINING {
        Style::Error
    } else if remaining_ratio <= LOW_REMAINING {
        Style::Warning
    } else {
        Style::Plain
    }
}

fn max_width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(display_width).max().unwrap_or(0)
}

pub fn render_pairs(pairs: &[(&str, Cell)], palette: &Palette) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let key_width = pairs
        .iter()
        .map(|(key, _)| display_width(key))
        .max()
        .unwrap_or(0);
    let mut output = String::new();
    for (key, cell) in pairs {
        let padded_key = pad(key, key_width, Align::Left);
        output.push_str(&palette.wrap(pair_key_style(key), &padded_key));
        output.push_str(&" ".repeat(KEY_VALUE_GAP));
        output.push_str(&palette.wrap(pair_value_style(key, cell), &cell.text));
        output.push('\n');
    }
    output
}

fn pair_key_style(key: &str) -> Style {
    match key {
        "WARNING" => Style::Warning,
        "LAST_ERROR" => Style::Error,
        _ => Style::Plain,
    }
}

fn pair_value_style(key: &str, cell: &Cell) -> Style {
    if matches!(cell.style, Style::Warning | Style::Error) {
        return cell.style;
    }
    match key {
        "WARNING" => Style::Warning,
        "LAST_ERROR" => Style::Error,
        "STATUS" if cell.text == "stopped" || cell.text == "stale" => Style::Error,
        _ => effective_style(cell),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorMode;
    use crate::summary::Currency;

    #[test]
    fn cjk_and_fullwidth_characters_use_two_columns() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("工作"), 4);
        assert_eq!(display_width("Ａ"), 2);
        assert_eq!(display_width("A中"), 3);
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("\u{1B000}"), 2);
        assert_eq!(display_width("か\u{3099}"), 2);
    }

    #[test]
    fn uncommon_cjk_and_combining_marks_keep_ascii_borders_aligned() {
        let table = render_table(
            &["LABEL", "N"],
            &[
                vec![Cell::new("\u{1B000}"), Cell::new("1")],
                vec![Cell::new("か\u{3099}"), Cell::new("2")],
            ],
            &Palette::off(),
        );
        let widths: Vec<usize> = table.lines().map(display_width).collect();
        assert!(
            widths.iter().all(|width| *width == widths[0]),
            "misaligned table:\n{table}"
        );
    }

    #[test]
    fn cjk_label_keeps_ascii_borders_aligned() {
        let table = render_table(
            &["LABEL", "N"],
            &[
                vec![Cell::new("中文标签"), Cell::new("1")],
                vec![Cell::new("ascii"), Cell::new("2")],
            ],
            &Palette::off(),
        );
        let widths: Vec<usize> = table.lines().map(display_width).collect();
        assert!(
            widths.iter().all(|width| *width == widths[0]),
            "misaligned table:\n{table}"
        );
        assert!(table.lines().next().unwrap().starts_with('+'));
        assert!(table.contains("| 中文标签 |"));
        assert!(!table.contains('\u{2500}'));
    }

    #[test]
    fn control_characters_in_cells_are_redacted() {
        for value in ["\x1b[31mred", "line\rfeed", "new\nline", "has\ttab"] {
            let table = render_table(&["LABEL"], &[vec![Cell::new(value)]], &Palette::off());
            assert_eq!(table.lines().count(), 5, "{value:?}\n{table}");
            assert!(table.contains("[redacted]"), "{table}");
            assert!(!table.contains('\x1b'), "{table}");
            assert!(!table.contains('\r'), "{table}");
            assert!(!table.contains('\t'), "{table}");
        }
    }

    #[test]
    fn empty_cells_render_as_dash() {
        let table = render_table(&["A"], &[vec![Cell::new("")]], &Palette::off());
        assert!(table.contains("| - |"), "{table}");
    }

    #[test]
    fn missing_numeric_limit_renders_as_right_aligned_dash() {
        let table = render_table(&["LIMIT"], &[vec![Cell::limit(None)]], &Palette::off());
        assert_eq!(
            table, "+-------+\n| LIMIT |\n+-------+\n|     - |\n+-------+\n",
            "{table}"
        );
    }

    #[test]
    fn numeric_columns_are_right_aligned() {
        let table = render_table(
            &["USED", "LIMIT"],
            &[vec![Cell::used(4.5, Some(20.0)), Cell::limit(Some(20.0))]],
            &Palette::off(),
        );
        assert!(
            table.contains("|  4.5 |    20 |") || table.contains("| 4.5 | 20 |"),
            "{table}"
        );
        let data = table.lines().nth(3).unwrap();
        assert!(data.trim_start_matches('|').contains(" 4.5 "), "{data}");
    }

    #[test]
    fn used_ratio_selects_error_and_warning_colors() {
        let palette = Palette { enabled: true };
        let high = render_table(&["USED"], &[vec![Cell::used(90.0, Some(100.0))]], &palette);
        let warn = render_table(&["USED"], &[vec![Cell::used(75.0, Some(100.0))]], &palette);
        let ok = render_table(&["USED"], &[vec![Cell::used(10.0, Some(100.0))]], &palette);
        assert!(high.contains(ERROR), "{high}");
        assert!(warn.contains(WARNING), "{warn}");
        assert!(!ok.lines().nth(3).unwrap().contains('\x1b'), "{ok}");
    }

    #[test]
    fn color_mode_respects_tty_and_no_color() {
        let unset = Palette::resolve(ColorMode::Auto, true, None);
        let empty = Palette::resolve(ColorMode::Auto, true, Some(OsStr::new("")));
        let blocked = Palette::resolve(ColorMode::Auto, true, Some(OsStr::new("1")));
        let notty = Palette::resolve(ColorMode::Auto, false, None);
        let always = Palette::resolve(ColorMode::Always, false, Some(OsStr::new("1")));
        let never = Palette::resolve(ColorMode::Never, true, None);
        assert!(unset.enabled());
        assert!(empty.enabled());
        assert!(!blocked.enabled());
        assert!(!notty.enabled());
        assert!(always.enabled());
        assert!(!never.enabled());
    }

    #[test]
    fn key_value_pairs_align_keys_without_a_border() {
        let block = render_pairs(
            &[
                ("STATUS", Cell::new("running")),
                ("CREDENTIAL_BACKEND", Cell::new("file_fallback")),
            ],
            &Palette::off(),
        );
        assert_eq!(
            block,
            "STATUS              running\nCREDENTIAL_BACKEND  file_fallback\n"
        );
        assert!(!block.contains('+'));
    }

    #[test]
    fn dash_values_are_dimmed_when_color_is_on() {
        let block = render_pairs(&[("PLAN", Cell::new(""))], &Palette { enabled: true });
        assert!(block.contains(DIM), "{block}");
        assert!(block.contains('-'), "{block}");
    }

    #[test]
    fn section_header_is_a_single_plain_line() {
        let line = render_section_header("ACCOUNT primary (claude)", &Palette::off());
        assert_eq!(line, "==== ACCOUNT primary (claude) ====\n");
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn section_header_uses_header_style_when_color_is_on() {
        let line = render_section_header("ACCOUNT primary (claude)", &Palette { enabled: true });
        assert_eq!(
            line,
            format!("{HEADER}==== ACCOUNT primary (claude) ===={RESET}\n")
        );
        assert!(line.starts_with(HEADER), "{line:?}");
        assert!(line.contains(RESET), "{line:?}");
    }

    fn summary_row(window: &str, metric: &str, value: SummaryValue) -> SummaryRow {
        SummaryRow {
            window: window.into(),
            metric: metric.into(),
            value,
            resets_at: None,
            remaining_ratio: None,
            disabled: false,
        }
    }

    fn trailing_progress_bar(line: &str) -> &str {
        &line[line.len() - BAR_RENDER_WIDTH..]
    }

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        use chrono::TimeZone as _;
        Utc.with_ymd_and_hms(2026, 8, 29, hour, minute, 0).unwrap()
    }

    #[test]
    fn compact_identity_omits_usage_and_shortens_models() {
        assert_eq!(compact_identity("5h", "usage"), "5h");
        assert_eq!(compact_identity("weekly", "usage"), "weekly");
        assert_eq!(compact_identity("fable", "usage"), "fable");
        assert_eq!(compact_identity("5h", "GPT-5.3"), "5h-5.3");
        assert_eq!(compact_identity("weekly", "GPT-5.3"), "weekly-5.3");
        assert_eq!(compact_identity("weekly", "Codex"), "weekly-Codex");
        assert_eq!(compact_identity("weekly", "GrokBuild"), "GrokBuild");
        assert_eq!(
            compact_identity("Resets", "available count"),
            "Resets  available count"
        );
    }

    #[test]
    fn summary_rows_put_the_progress_bar_last_and_align_the_columns() {
        let now = at(12, 0);
        let rows = vec![
            SummaryRow {
                resets_at: Some(at(15, 57)),
                remaining_ratio: Some(0.97),
                ..summary_row("5h", "usage", SummaryValue::Remains(97.0))
            },
            SummaryRow {
                resets_at: Some(at(20, 30)),
                remaining_ratio: Some(1.0),
                ..summary_row("weekly", "GPT-5.3", SummaryValue::Remains(100.0))
            },
            SummaryRow {
                resets_at: Some(at(20, 30)),
                remaining_ratio: Some(0.5),
                ..summary_row("weekly", "Codex", SummaryValue::Remains(50.0))
            },
            SummaryRow {
                remaining_ratio: Some(0.4),
                ..summary_row("weekly", "GrokBuild", SummaryValue::Remains(40.0))
            },
        ];

        let block = render_summary_rows(&rows, now, &Palette::off());
        assert_eq!(
            block,
            concat!(
                "5h            remains  97%  resets in 3h57m  [##########]\n",
                "weekly-5.3    remains 100%  resets in 8h30m  [##########]\n",
                "weekly-Codex  remains  50%  resets in 8h30m  [-----#####]\n",
                "GrokBuild     remains  40%                   [------####]\n",
            ),
            "{block}"
        );
        for line in block.lines() {
            let bar = trailing_progress_bar(line);
            assert!(
                bar.chars()
                    .skip(1)
                    .take(BAR_WIDTH)
                    .all(|c| c == BAR_FILLED || c == BAR_EMPTY),
                "nothing may follow the bar: {line}"
            );
        }
    }

    #[test]
    fn bars_share_one_column_even_when_a_row_has_no_reset_time() {
        let block = render_summary_rows(
            &[
                SummaryRow {
                    resets_at: Some(at(15, 57)),
                    remaining_ratio: Some(0.97),
                    ..summary_row("5h", "usage", SummaryValue::Remains(97.0))
                },
                SummaryRow {
                    remaining_ratio: Some(0.5),
                    ..summary_row("monthly", "usage", SummaryValue::Remains(50.0))
                },
            ],
            at(12, 0),
            &Palette::off(),
        );

        let starts: Vec<usize> = block
            .lines()
            .map(|line| display_width(&line[..line.len() - BAR_RENDER_WIDTH]))
            .collect();
        assert_eq!(starts[0], starts[1], "{block}");
    }

    #[test]
    fn a_row_without_a_known_ratio_draws_no_bar_and_ends_at_its_value() {
        let block = render_summary_rows(
            &[
                summary_row(
                    "Credits",
                    "credit balance",
                    SummaryValue::Balance {
                        amount: 0.0,
                        currency: Currency { code: "USD".into() },
                    },
                ),
                summary_row("Credits", "reset", SummaryValue::CreditsUnlimited),
            ],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(
            block,
            "Credits  credit balance  balance $0.00\nCredits  reset           credits unlimited\n",
            "{block}"
        );
        assert!(!block.lines().any(|line| line.ends_with(']')), "{block}");
    }

    #[test]
    fn credits_with_a_positive_limit_show_of_and_a_bar() {
        let block = render_summary_rows(
            &[SummaryRow {
                remaining_ratio: Some(0.715),
                ..summary_row(
                    "monthly",
                    "usage",
                    SummaryValue::Credits {
                        used: 285.0,
                        limit: Some(1000.0),
                    },
                )
            }],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(
            block, "monthly  credits 285 of 1000  [---#######]\n",
            "{block}"
        );
    }

    #[test]
    fn credits_without_a_limit_keep_a_bare_count_and_no_bar() {
        let block = render_summary_rows(
            &[summary_row(
                "Credits",
                "credit balance",
                SummaryValue::Credits {
                    used: 0.0,
                    limit: None,
                },
            )],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(block, "Credits  credit balance  credits 0\n", "{block}");
        assert!(!block.contains(']'), "{block}");
    }

    #[test]
    fn spent_money_shows_the_amount_and_no_bar() {
        let block = render_summary_rows(
            &[summary_row(
                "monthly",
                "total spend",
                SummaryValue::Spent {
                    amount: 925.11,
                    limit: 400.0,
                    currency: Currency { code: "USD".into() },
                },
            )],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(
            block, "monthly  total spend  spent $925.11 of $400.00\n",
            "{block}"
        );
        assert!(!block.contains(']'), "{block}");
    }

    #[test]
    fn a_disabled_row_is_marked_without_displacing_the_bar() {
        let block = render_summary_rows(
            &[SummaryRow {
                remaining_ratio: Some(0.85),
                disabled: true,
                ..summary_row("monthly", "auto", SummaryValue::Remains(85.0))
            }],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(
            block, "monthly  auto  remains 85% (off)  [-#########]\n",
            "{block}"
        );
    }

    #[test]
    fn non_finite_amounts_render_as_a_dash_rather_than_nan() {
        let block = render_summary_rows(
            &[
                summary_row("5h", "usage", SummaryValue::Remains(f64::NAN)),
                summary_row(
                    "monthly",
                    "spend",
                    SummaryValue::Balance {
                        amount: f64::INFINITY,
                        currency: Currency { code: "USD".into() },
                    },
                ),
            ],
            at(12, 0),
            &Palette::off(),
        );

        assert!(!block.contains("NaN"), "{block}");
        assert!(!block.contains("inf"), "{block}");
        assert!(block.contains("remains -"), "{block}");
        assert!(block.contains("balance -"), "{block}");
    }

    #[test]
    fn an_unknown_currency_falls_back_to_its_code() {
        let block = render_summary_rows(
            &[summary_row(
                "monthly",
                "spend",
                SummaryValue::Balance {
                    amount: 12.5,
                    currency: Currency { code: "SEK".into() },
                },
            )],
            at(12, 0),
            &Palette::off(),
        );
        assert!(block.contains("balance SEK 12.50"), "{block}");
    }

    #[test]
    fn a_counted_unit_reports_the_count_rather_than_a_quota() {
        let block = render_summary_rows(
            &[summary_row(
                "5h",
                "requests",
                SummaryValue::Counted {
                    used: 12.0,
                    limit: None,
                },
            )],
            at(12, 0),
            &Palette::off(),
        );
        assert_eq!(block, "5h  requests  used 12\n", "{block}");
    }

    #[test]
    fn the_bar_turns_yellow_then_red_as_the_remaining_quota_falls() {
        let palette = Palette { enabled: true };
        let bar = |ratio: f64| {
            render_summary_rows(
                &[SummaryRow {
                    remaining_ratio: Some(ratio),
                    ..summary_row("5h", "usage", SummaryValue::Remains(ratio * 100.0))
                }],
                at(12, 0),
                &palette,
            )
        };
        assert!(bar(0.10).contains(ERROR), "{}", bar(0.10));
        assert!(bar(0.25).contains(WARNING), "{}", bar(0.25));
        let healthy = bar(0.26);
        assert!(!healthy.contains('\u{1b}'), "{healthy}");
        assert!(
            !render_summary_rows(
                &[SummaryRow {
                    remaining_ratio: Some(0.05),
                    ..summary_row("5h", "usage", SummaryValue::Remains(5.0))
                }],
                at(12, 0),
                &Palette::off(),
            )
            .contains('\u{1b}'),
            "color never must stay plain"
        );
    }

    #[test]
    fn summary_rows_redact_control_characters_in_provider_strings() {
        let block = render_summary_rows(
            &[SummaryRow {
                remaining_ratio: Some(0.20),
                ..summary_row(
                    "boss\n==== ACCOUNT forged (claude) ====",
                    "spend\r==========",
                    SummaryValue::Balance {
                        amount: 1.0,
                        currency: Currency {
                            code: "US\u{1b}[0mD".into(),
                        },
                    },
                )
            }],
            at(12, 0),
            &Palette { enabled: true },
        );

        assert_eq!(block.lines().count(), 1, "{block}");
        assert!(!block.contains("forged"), "{block}");
        assert!(!block.contains("===="), "{block}");
        assert!(!block.contains('\r'), "{block}");
        assert!(
            block.contains("[--------##]"),
            "only the trailing bar may use bar fill characters:\n{block}"
        );
        assert!(!block.contains('='), "{block}");
        assert!(block.contains("[redacted] 1.00"), "{block}");
        assert_eq!(
            block.matches('\u{1b}').count(),
            2,
            "only the bar may be wrapped in ANSI:\n{block:?}"
        );
    }

    #[test]
    fn relative_times_shrink_from_days_to_a_sub_minute_floor() {
        let now = at(12, 0);
        assert_eq!(
            relative_future(now + chrono::Duration::minutes(8130), now),
            "in 5d15h"
        );
        assert_eq!(
            relative_future(now + chrono::Duration::minutes(237), now),
            "in 3h57m"
        );
        assert_eq!(
            relative_future(now + chrono::Duration::minutes(12), now),
            "in 12m"
        );
        assert_eq!(
            relative_future(now + chrono::Duration::seconds(30), now),
            "in <1m"
        );
        assert_eq!(relative_future(now, now), "now");
        assert_eq!(
            relative_future(now - chrono::Duration::hours(1), now),
            "now"
        );
        assert_eq!(
            relative_past(now - chrono::Duration::minutes(2), now),
            "2m ago"
        );
        assert_eq!(
            relative_past(now + chrono::Duration::hours(1), now),
            "<1m ago"
        );
    }

    #[test]
    fn a_standalone_line_is_sanitized_before_it_is_wrapped() {
        let line = render_line(
            "! stale\u{1b}[31m",
            Style::Warning,
            &Palette { enabled: true },
        );
        assert_eq!(line, format!("{WARNING}[redacted]{RESET}\n"));
    }

    #[test]
    fn section_header_redacts_control_characters_before_wrapping() {
        let palette = Palette { enabled: true };
        for value in ["\x1b[31mred", "line\rfeed", "new\nline", "has\ttab"] {
            let line = render_section_header(value, &palette);
            assert_eq!(
                line,
                format!("{HEADER}==== [redacted] ===={RESET}\n"),
                "{value:?}\n{line:?}"
            );
            assert_eq!(line.lines().count(), 1, "{value:?}\n{line}");
            assert!(!line.contains("==== ACCOUNT"), "{line}");
            assert_eq!(
                line.matches('\x1b').count(),
                2,
                "only palette wrap may emit ANSI:\n{line:?}"
            );
        }
    }
}
