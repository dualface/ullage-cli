//! ASCII tables and aligned key-value blocks for human-readable CLI output.
//!
//! Color is applied only after cell text has been sanitized. Untrusted provider
//! strings never become part of an escape sequence.

use std::collections::HashSet;
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
/// Where a reset stops being told in hours and starts being told in days.
const TWO_DAYS_IN_SECONDS: i64 = 2 * 24 * 60 * 60;
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

/// Shared column widths for the four summary fields: identity, reading,
/// reset time, and progress bar.
///
/// `show --all` measures every account first, then renders each block with
/// the same layout so those fields line up across section headers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SummaryLayout {
    identity_width: usize,
    verb_width: usize,
    amount_width: usize,
    suffix_width: usize,
    resets_width: usize,
}

impl SummaryLayout {
    fn from_cells(cells: &[SummaryCells]) -> Self {
        Self {
            identity_width: max_width(cells.iter().map(|cell| cell.identity.as_str())),
            verb_width: max_width(cells.iter().map(|cell| cell.verb)),
            amount_width: max_width(cells.iter().map(|cell| cell.amount.as_str())),
            suffix_width: max_width(cells.iter().map(|cell| cell.suffix)),
            resets_width: max_width(cells.iter().map(|cell| cell.resets.as_str())),
        }
    }

    /// Widens each column to the larger of `self` and `other`.
    pub fn expand(&mut self, other: Self) {
        self.identity_width = self.identity_width.max(other.identity_width);
        self.verb_width = self.verb_width.max(other.verb_width);
        self.amount_width = self.amount_width.max(other.amount_width);
        self.suffix_width = self.suffix_width.max(other.suffix_width);
        self.resets_width = self.resets_width.max(other.resets_width);
    }
}

/// Measures the four summary columns for `rows` without rendering them.
pub fn measure_summary_layout(rows: &[SummaryRow], now: DateTime<Utc>) -> SummaryLayout {
    SummaryLayout::from_cells(&collect_summary_cells(rows, now))
}

/// Renders the summary grid: identity, reading, reset, progress bar.
///
/// Window and metric collapse into one identity cell (for example `5h`,
/// `5h-5.3`, or `GrokBuild`) so the table does not repeat `usage` or a full
/// model name. Verb, amount, and suffix are fixed-width fields: empty verbs
/// still occupy the verb column, and amounts are left-aligned so `$12.50`,
/// `85%`, and `1` start on the same column. The bar is always last so every
/// row shares one aligned slot, including rows without a reset time.
pub fn render_summary_rows(rows: &[SummaryRow], now: DateTime<Utc>, palette: &Palette) -> String {
    let cells = collect_summary_cells(rows, now);
    render_summary_cells(&cells, &SummaryLayout::from_cells(&cells), palette)
}

/// Renders `rows` using a layout measured from a larger set of accounts.
pub fn render_summary_rows_aligned(
    rows: &[SummaryRow],
    now: DateTime<Utc>,
    palette: &Palette,
    layout: &SummaryLayout,
) -> String {
    render_summary_cells(&collect_summary_cells(rows, now), layout, palette)
}

fn collect_summary_cells(rows: &[SummaryRow], now: DateTime<Utc>) -> Vec<SummaryCells> {
    let rows: Vec<&SummaryRow> = rows.iter().filter(|row| !hidden_in_table(row)).collect();
    let named_windows: HashSet<String> = rows
        .iter()
        .filter_map(|row| {
            let window = visible_cell_text(&row.window);
            let metric = visible_cell_text(&row.metric);
            if metric.is_empty() || metric == "usage" {
                return None;
            }
            if metric.eq_ignore_ascii_case("GrokBuild") {
                return None;
            }
            Some(window)
        })
        .collect();
    rows.iter()
        .map(|row| summary_cells(row, now, &named_windows))
        .collect()
}

fn render_summary_cells(
    cells: &[SummaryCells],
    layout: &SummaryLayout,
    palette: &Palette,
) -> String {
    let readings: Vec<String> = cells
        .iter()
        .map(|cell| {
            format_reading(
                cell,
                layout.verb_width,
                layout.amount_width,
                layout.suffix_width,
            )
        })
        .collect();
    let reading_width = max_width(readings.iter().map(String::as_str));

    let gap = " ".repeat(COLUMN_GAP);
    let mut output = String::new();
    for (cell, reading) in cells.iter().zip(&readings) {
        let mut columns = vec![pad(&cell.identity, layout.identity_width, Align::Left)];
        if reading_width > 0 {
            columns.push(pad(reading, reading_width, Align::Left));
        }
        if layout.resets_width > 0 {
            columns.push(pad(&cell.resets, layout.resets_width, Align::Left));
        }
        let mut line = columns.join(&gap);
        if let Some(ratio) = cell.remaining_ratio {
            line.push_str(&gap);
            line.push_str(&palette.wrap(bar_style(ratio), &progress_bar(ratio)));
        } else {
            // Empty reset padding is only needed to hold the bar column.
            line = line.trim_end().to_owned();
        }
        output.push_str(&line);
        output.push('\n');
    }
    output
}

fn format_reading(
    cell: &SummaryCells,
    verb_width: usize,
    amount_width: usize,
    suffix_width: usize,
) -> String {
    let mut parts = Vec::new();
    if verb_width > 0 {
        parts.push(pad(cell.verb, verb_width, Align::Left));
    }
    if amount_width > 0 {
        // Left-align mixed units (`$12.50` vs `85%` vs `1`). Right-aligning
        // them to the longest amount only shoves the shorter values around.
        parts.push(pad(&cell.amount, amount_width, Align::Left));
    }
    if suffix_width > 0 {
        parts.push(pad(cell.suffix, suffix_width, Align::Left));
    }
    parts.join(" ")
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

fn summary_cells(
    row: &SummaryRow,
    now: DateTime<Utc>,
    leftover_windows: &HashSet<String>,
) -> SummaryCells {
    let window = visible_cell_text(&row.window);
    let metric = visible_cell_text(&row.metric);
    let (verb, amount) = summary_value_text(&row.value);
    SummaryCells {
        identity: compact_identity(&window, &metric, leftover_windows.contains(&window)),
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
fn compact_identity(window: &str, metric: &str, keep_usage: bool) -> String {
    let metric = metric.trim();
    if window.eq_ignore_ascii_case("resets") || window.eq_ignore_ascii_case("reset") {
        if metric.is_empty() || metric == "available count" {
            return "Reset".into();
        }
    }
    if window.eq_ignore_ascii_case("credits") && (metric.is_empty() || metric == "credit balance") {
        return "Balance".into();
    }
    if metric.eq_ignore_ascii_case("GrokBuild") {
        return "GrokBuild".into();
    }
    if metric == "total spend" {
        return format!("{window} spend");
    }
    if metric.is_empty() || metric == "usage" {
        if metric == "usage" && keep_usage {
            return format!("{window}-usage");
        }
        return window.to_string();
    }
    if let Some(rest) = strip_gpt_prefix(metric) {
        return format!("{window}-{rest}");
    }
    if matches!(metric, "Codex" | "auto" | "api") {
        return format!("{window}-{metric}");
    }
    format!("{window}{}{metric}", " ".repeat(COLUMN_GAP))
}

fn hidden_in_table(row: &SummaryRow) -> bool {
    let on_demand = row.metric == "on demand" || row.metric.starts_with("on demand ");
    on_demand && (row.disabled || matches!(row.value, SummaryValue::Disabled))
}

fn strip_gpt_prefix(metric: &str) -> Option<&str> {
    let rest = metric
        .strip_prefix("GPT-")
        .or_else(|| metric.strip_prefix("gpt-"))?;
    (!rest.is_empty()).then_some(rest)
}

fn summary_value_text(value: &SummaryValue) -> (&'static str, String) {
    match value {
        SummaryValue::Remains(percent) => remains_text(*percent),
        SummaryValue::Used(percent) => ("used", format_percent(*percent)),
        SummaryValue::Balance { amount, currency } => {
            ("balance", format_money(*amount, &currency.code))
        }
        SummaryValue::Spent {
            amount, currency, ..
        } => ("", format_money(*amount, &currency.code)),
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

/// How much is left, in words when the number alone would mislead. `remains
/// 0%` reads like a measurement that came back empty rather than like quota
/// that is gone, and since the percentage is rounded it also covers everything
/// under half a percent, which is still usable.
fn remains_text(percent: f64) -> (&'static str, String) {
    if !percent.is_finite() {
        return ("remains", format_percent(percent));
    }
    if percent <= 0.0 {
        return ("used up", String::new());
    }
    if percent < 0.5 {
        return ("remains", "<1%".into());
    }
    ("remains", format_percent(percent))
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
    format!("in {}", reset_duration_text(seconds))
}

/// Describes how far `moment` is behind `now`, e.g. `2m ago`.
pub fn relative_past(moment: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - moment).num_seconds().max(0);
    format!("{} ago", duration_text(seconds))
}

/// How long until a window resets. Days only take over past two of them:
/// `in 1d15h` is readable, but a reset the table rounds to `1d` hides whether
/// the wait is 25 hours or 47, and that decides whether a limit is worth
/// waiting out. Under an hour this falls back to the shared shape, since
/// minutes are all that is left to say.
fn reset_duration_text(seconds: i64) -> String {
    if seconds >= TWO_DAYS_IN_SECONDS {
        return duration_text(seconds);
    }
    // Round to the nearest minute, so 59.7 minutes carries into the hour
    // rather than printing as `0h60m`.
    let minutes = (seconds + 30) / 60;
    let hours = minutes / 60;
    if hours == 0 {
        return duration_text(seconds);
    }
    match minutes % 60 {
        0 => format!("{hours}h"),
        rest => format!("{hours}h{rest:02}m"),
    }
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
        assert_eq!(compact_identity("5h", "usage", false), "5h");
        assert_eq!(compact_identity("weekly", "usage", false), "weekly");
        assert_eq!(compact_identity("fable", "usage", false), "fable");
        assert_eq!(compact_identity("5h", "GPT-5.3", false), "5h-5.3");
        assert_eq!(compact_identity("weekly", "GPT-5.3", false), "weekly-5.3");
        assert_eq!(compact_identity("weekly", "Codex", false), "weekly-Codex");
        assert_eq!(compact_identity("weekly", "GrokBuild", false), "GrokBuild");
        assert_eq!(compact_identity("monthly", "usage", true), "monthly-usage");
        assert_eq!(compact_identity("monthly", "auto", false), "monthly-auto");
        assert_eq!(compact_identity("monthly", "api", false), "monthly-api");
        assert_eq!(
            compact_identity("monthly", "total spend", false),
            "monthly spend"
        );
        assert_eq!(
            compact_identity("Resets", "available count", false),
            "Reset"
        );
        assert_eq!(
            compact_identity("Credits", "credit balance", false),
            "Balance"
        );
    }

    #[test]
    fn usage_stays_when_the_same_window_has_other_metrics() {
        let block = render_summary_rows(
            &[
                summary_row(
                    "monthly",
                    "total spend",
                    SummaryValue::Spent {
                        amount: 3.0,
                        limit: 20.0,
                        currency: Currency { code: "USD".into() },
                    },
                ),
                SummaryRow {
                    remaining_ratio: Some(0.73),
                    ..summary_row("monthly", "usage", SummaryValue::Remains(73.0))
                },
            ],
            at(12, 0),
            &Palette::off(),
        );
        assert!(block.contains("monthly-usage"), "{block}");
        assert!(block.contains("monthly spend"), "{block}");
    }

    #[test]
    fn spent_quota_reads_as_used_up_rather_than_zero_percent() {
        assert_eq!(remains_text(73.0), ("remains", "73%".into()));
        // Rounds to 1%, so the number still carries it.
        assert_eq!(remains_text(0.6), ("remains", "1%".into()));
        // Would round to 0% while quota is left: say how small it is instead.
        assert_eq!(remains_text(0.4), ("remains", "<1%".into()));
        assert_eq!(remains_text(0.0), ("used up", String::new()));
        assert_eq!(remains_text(-5.0), ("used up", String::new()));
        assert_eq!(remains_text(f64::NAN), ("remains", "-".into()));

        let block = render_summary_rows(
            &[SummaryRow {
                remaining_ratio: Some(0.0),
                ..summary_row("weekly", "usage", SummaryValue::Remains(0.0))
            }],
            at(12, 0),
            &Palette::off(),
        );
        assert!(block.contains("used up"), "{block}");
        assert!(!block.contains("0%"), "{block}");
        // The row still carries its bar, empty.
        assert!(block.contains("[----------]"), "{block}");
    }

    #[test]
    fn resets_stay_in_hours_and_minutes_until_two_days() {
        let now = at(12, 0);
        let ahead = |minutes: i64| relative_future(now + chrono::Duration::minutes(minutes), now);
        // Whole hours drop the minutes rather than printing `3h00m`.
        assert_eq!(ahead(180), "in 3h");
        assert_eq!(ahead(210), "in 3h30m");
        // Past a day, still hours: `in 1d` would hide 25 hours against 47.
        assert_eq!(ahead(25 * 60 + 1), "in 25h01m");
        assert_eq!(ahead(47 * 60 + 59), "in 47h59m");
        // Two days and beyond, days again.
        assert_eq!(ahead(48 * 60), "in 2d00h");
        assert_eq!(ahead(8130), "in 5d15h");
        // Under an hour is unchanged.
        assert_eq!(ahead(12), "in 12m");
        assert_eq!(
            relative_future(now + chrono::Duration::seconds(30), now),
            "in <1m"
        );
        // Seconds round into the minute, and a full minute into the hour.
        assert_eq!(
            relative_future(now + chrono::Duration::seconds(3_600 + 31), now),
            "in 1h01m"
        );
        assert_eq!(
            relative_future(now + chrono::Duration::seconds(3_600 + 59 * 60 + 45), now),
            "in 2h"
        );
        // "updated 30h ago" is a different question and keeps the old shape.
        assert_eq!(
            relative_past(now - chrono::Duration::minutes(30 * 60), now),
            "1d06h ago"
        );
    }

    #[test]
    fn disabled_on_demand_rows_are_omitted() {
        let block = render_summary_rows(
            &[
                summary_row("monthly", "auto", SummaryValue::Remains(85.0)),
                summary_row("monthly", "on demand", SummaryValue::Disabled),
                SummaryRow {
                    disabled: true,
                    ..summary_row(
                        "monthly",
                        "on demand spend",
                        SummaryValue::Spent {
                            amount: 0.0,
                            limit: 50.0,
                            currency: Currency { code: "USD".into() },
                        },
                    )
                },
            ],
            at(12, 0),
            &Palette::off(),
        );
        assert!(block.contains("monthly-auto"), "{block}");
        assert!(!block.contains("on demand"), "{block}");
        assert!(!block.contains("disabled"), "{block}");
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
                "5h            remains 97%   resets in 3h57m  [##########]\n",
                "weekly-5.3    remains 100%  resets in 8h30m  [##########]\n",
                "weekly-Codex  remains 50%   resets in 8h30m  [-----#####]\n",
                "GrokBuild     remains 40%                    [------####]\n",
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
    fn mixed_readings_share_amount_reset_and_bar_columns() {
        let block = render_summary_rows(
            &[
                SummaryRow {
                    resets_at: Some(at(16, 0)),
                    remaining_ratio: Some(1.0),
                    ..summary_row("5h", "GPT-5.3", SummaryValue::Remains(100.0))
                },
                summary_row(
                    "Credits",
                    "credit balance",
                    SummaryValue::Credits {
                        used: 1.0,
                        limit: None,
                    },
                ),
                SummaryRow {
                    resets_at: Some(at(16, 0)),
                    ..summary_row(
                        "monthly",
                        "total spend",
                        SummaryValue::Spent {
                            amount: 12.5,
                            limit: 20.0,
                            currency: Currency { code: "USD".into() },
                        },
                    )
                },
            ],
            at(12, 0),
            &Palette::off(),
        );
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 3, "{block}");
        let resets: Vec<usize> = lines
            .iter()
            .filter_map(|line| line.find("resets"))
            .collect();
        assert_eq!(resets.len(), 2, "{block}");
        assert_eq!(resets[0], resets[1], "{block}");
        let amounts = [(lines[0], "100%"), (lines[1], "1"), (lines[2], "$12.50")];
        let amount_starts: Vec<usize> = amounts
            .iter()
            .map(|(line, amount)| {
                line.find(amount)
                    .unwrap_or_else(|| panic!("{amount} missing from {line}"))
            })
            .collect();
        assert_eq!(amount_starts[0], amount_starts[1], "{block}");
        assert_eq!(amount_starts[0], amount_starts[2], "{block}");
        let bar_at = lines[0].find('[').expect("{block}");
        assert!(
            !lines[1].contains('[') && lines[1].len() <= bar_at,
            "rows without a bar must not overrun the bar column: {block}"
        );
        assert!(
            !lines[2].contains('[') && lines[2].len() <= bar_at,
            "rows without a bar must not overrun the bar column: {block}"
        );
    }

    #[test]
    fn summary_columns_align_across_accounts() {
        let now = at(12, 0);
        let short = [SummaryRow {
            resets_at: Some(at(12, 40)),
            remaining_ratio: Some(0.17),
            ..summary_row("5h", "usage", SummaryValue::Remains(17.0))
        }];
        let long = [
            SummaryRow {
                resets_at: Some(at(16, 0)),
                remaining_ratio: Some(0.04),
                ..summary_row("weekly", "Codex", SummaryValue::Remains(4.0))
            },
            SummaryRow {
                resets_at: Some(at(23, 0)),
                ..summary_row(
                    "monthly",
                    "total spend",
                    SummaryValue::Spent {
                        amount: 953.12,
                        limit: 400.0,
                        currency: Currency { code: "USD".into() },
                    },
                )
            },
        ];
        let mut layout = measure_summary_layout(&short, now);
        layout.expand(measure_summary_layout(&long, now));
        let claude = render_summary_rows_aligned(&short, now, &Palette::off(), &layout);
        let cursor = render_summary_rows_aligned(&long, now, &Palette::off(), &layout);
        let five_hours = claude.lines().next().expect(&claude);
        let weekly = cursor.lines().next().expect(&cursor);
        let spend = cursor.lines().nth(1).expect(&cursor);
        assert_eq!(
            five_hours.find("remains"),
            weekly.find("remains"),
            "{claude}{cursor}"
        );
        assert_eq!(
            five_hours.find("17%"),
            weekly.find("4%"),
            "{claude}{cursor}"
        );
        assert_eq!(
            five_hours.find("17%"),
            spend.find("$953.12"),
            "{claude}{cursor}"
        );
        assert_eq!(
            five_hours.find("resets"),
            weekly.find("resets"),
            "{claude}{cursor}"
        );
        assert_eq!(
            five_hours.find("resets"),
            spend.find("resets"),
            "{claude}{cursor}"
        );
        assert_eq!(five_hours.find('['), weekly.find('['), "{claude}{cursor}");
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
            block, "Balance         balance $0.00\nCredits  reset  credits unlimited\n",
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
        assert_eq!(block, "Balance  credits 0\n", "{block}");
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
        assert_eq!(block, "monthly spend  $925.11\n", "{block}");
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
            block, "monthly-auto  remains 85% (off)  [-#########]\n",
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
