//! Full-screen subscription usage view.
//!
//! The view shows the same readings as `show`, in the same words, but packed
//! for a small screen: one highlighted title line per subscription instead of
//! a box, and a per-card column layout that drops its widest fields first so
//! the identity, the reading, and the reset countdown survive on a phone.

use std::io;

use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ullage_core::QueryOutcome;
use ullage_core::summary::UsageSummary;
use ullage_protocol::{
    CONTROL_PROTOCOL_VERSION, ControlRequest, ControlResult, SnapshotPayload, SubscriptionUsage,
};

use crate::errors::{error_output, result_exit_code, sanitize_partial_failure_controls};
use crate::render::{MetricFilterChoice, RenderView, render_result};
use crate::table::{
    BAR_RENDER_WIDTH, Severity, SummaryCells, collect_summary_cells, countdown_text, progress_bar,
    remaining_severity,
};
use crate::{
    ClientError, ColorMode, Command, ControlClient, ExitCode, OutputFormat, RunOutput,
    daemon_upgrade_notice, next_request_id, response_matches_command, sanitize_cell,
    to_control_command,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const PREFERRED_CARD_WIDTH: u16 = 42;
const HORIZONTAL_GAP: u16 = 2;
/// Cards need no blank row between them: the reversed title line separates them.
const VERTICAL_GAP: u16 = 0;
/// One space between the fields of a row, half of what the table uses.
const FIELD_GAP: usize = 1;
/// Rows a single wheel notch scrolls.
const WHEEL_LINES: u16 = 3;

/// Loads all snapshots, then owns the terminal until the user exits.
pub fn run_tui(client: &dyn ControlClient) -> RunOutput {
    let command = Command::Tui;
    let request_id = next_request_id();
    let request = ControlRequest::new(&request_id, to_control_command(&command));
    let mut response = match client.send(&request) {
        Ok(response) => response,
        Err(ClientError::DaemonUnavailable) => {
            return error_output(
                ExitCode::DaemonUnavailable,
                "daemon_unavailable",
                OutputFormat::Table,
            );
        }
        Err(ClientError::InvalidEndpoint) => {
            return error_output(
                ExitCode::Usage,
                "invalid_control_socket",
                OutputFormat::Table,
            );
        }
        Err(_) => {
            return error_output(
                ExitCode::ProtocolError,
                "invalid_daemon_response",
                OutputFormat::Table,
            );
        }
    };
    sanitize_partial_failure_controls(&mut response.result);
    if response.version != CONTROL_PROTOCOL_VERSION
        || response.request_id != request_id
        || response.diagnostic.is_some()
        || !response_matches_command(&command, &response.result)
    {
        return error_output(
            ExitCode::ProtocolError,
            "invalid_daemon_response",
            OutputFormat::Table,
        );
    }
    let upgrade_notice = daemon_upgrade_notice(response.daemon_version.as_deref())
        .map(|notice| format!("{notice}\n"))
        .unwrap_or_default();
    let snapshots = match response.result {
        ControlResult::Snapshots(snapshots) => snapshots,
        result => {
            let metric_choice = MetricFilterChoice::default();
            return render_result(
                result,
                OutputFormat::Table,
                false,
                false,
                false,
                ColorMode::Never,
                &RenderView {
                    metric_choice: &metric_choice,
                    account_with_metrics: false,
                },
            );
        }
    };

    match run_terminal(&snapshots) {
        Ok(()) => RunOutput {
            stdout: String::new(),
            stderr: upgrade_notice,
            code: snapshots_exit_code(&snapshots),
        },
        Err(_) => error_output(ExitCode::Failure, "tui_failed", OutputFormat::Table),
    }
}

fn snapshots_exit_code(snapshots: &[SnapshotPayload]) -> ExitCode {
    if snapshots.iter().any(|snapshot| {
        matches!(
            snapshot.usage,
            QueryOutcome::Partial { ref failures, .. } if !failures.is_empty()
        )
    }) {
        ExitCode::Partial
    } else {
        result_exit_code(&ControlResult::Snapshots(snapshots.to_vec()))
    }
}

fn run_terminal(snapshots: &[SnapshotPayload]) -> io::Result<()> {
    let _session = TerminalSession::enter(CrosstermControl)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut scroll = Scroll::default();
    loop {
        // The countdowns are rebuilt on every draw so a view left open does
        // not keep showing the wait measured when it was opened.
        let cards = cards(snapshots, Utc::now());
        terminal.draw(|frame| render(frame, &cards, &mut scroll))?;
        let action = match event::read()? {
            Event::Key(key) => key_action(key),
            Event::Mouse(mouse) => mouse_action(mouse),
            _ => Action::Ignore,
        };
        if action == Action::Quit {
            return Ok(());
        }
        scroll.pending = action;
    }
}

/// What the next event asks the view to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Action {
    Quit,
    Up(u16),
    Down(u16),
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    #[default]
    Ignore,
}

fn key_action(key: KeyEvent) -> Action {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Action::Ignore;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c' | 'C') => Action::Quit,
            _ => Action::Ignore,
        };
    }
    match key.code {
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up(1),
        KeyCode::Down | KeyCode::Char('j') => Action::Down(1),
        KeyCode::PageUp => Action::HalfUp,
        KeyCode::PageDown | KeyCode::Char(' ') => Action::HalfDown,
        KeyCode::Home | KeyCode::Char('g') => Action::Top,
        KeyCode::End | KeyCode::Char('G') => Action::Bottom,
        _ => Action::Ignore,
    }
}

fn mouse_action(mouse: MouseEvent) -> Action {
    match mouse.kind {
        MouseEventKind::ScrollUp => Action::Up(WHEEL_LINES),
        MouseEventKind::ScrollDown => Action::Down(WHEEL_LINES),
        _ => Action::Ignore,
    }
}

/// The first visible content row, plus the action waiting for the next draw.
///
/// An action is applied while drawing because only then are the viewport
/// height and the content height known, and both bound the offset.
#[derive(Clone, Copy, Debug, Default)]
struct Scroll {
    offset: u16,
    pending: Action,
}

impl Scroll {
    fn apply(&mut self, viewport_height: u16, content_height: u16) {
        let max_offset = content_height.saturating_sub(viewport_height);
        // At least one row, so a one-row viewport still moves.
        let half = (viewport_height / 2).max(1);
        self.offset = match std::mem::take(&mut self.pending) {
            Action::Up(lines) => self.offset.saturating_sub(lines),
            Action::Down(lines) => self.offset.saturating_add(lines),
            Action::HalfUp => self.offset.saturating_sub(half),
            Action::HalfDown => self.offset.saturating_add(half),
            Action::Top => 0,
            Action::Bottom => max_offset,
            Action::Quit | Action::Ignore => self.offset,
        }
        .min(max_offset);
    }
}

fn render(frame: &mut ratatui::Frame<'_>, cards: &[Card], scroll: &mut Scroll) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }
    if cards.is_empty() {
        frame.render_widget(
            Paragraph::new("No subscription snapshots").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    // The status line only earns its row once there are two.
    let (viewport, status) = match area.height {
        1 => (area, None),
        _ => (
            Rect::new(area.x, area.y, area.width, area.height - 1),
            Some(Rect::new(area.x, area.bottom() - 1, area.width, 1)),
        ),
    };
    let heights = cards.iter().map(Card::height).collect::<Vec<_>>();
    let rects = card_rects(area.width, &heights);
    let content = content_height(&rects);
    scroll.apply(viewport.height, content);
    for (card, rect) in cards.iter().zip(rects) {
        render_card(frame, card, rect, viewport, scroll.offset);
    }
    if let Some(status) = status {
        frame.render_widget(
            Paragraph::new(status_line(
                scroll.offset,
                viewport.height,
                content,
                status.width,
            )),
            status,
        );
    }
}

/// Draws the lines of one card that the scrolled viewport still shows.
fn render_card(
    frame: &mut ratatui::Frame<'_>,
    card: &Card,
    rect: Rect,
    viewport: Rect,
    offset: u16,
) {
    if rect.width == 0 {
        return;
    }
    for (index, line) in card_lines(card, rect.width).into_iter().enumerate() {
        let row = i64::from(rect.y) + index as i64 - i64::from(offset);
        if row < 0 || row >= i64::from(viewport.height) {
            continue;
        }
        let y = viewport.y + row as u16;
        frame.render_widget(Paragraph::new(line), Rect::new(rect.x, y, rect.width, 1));
    }
}

/// Renders a card as its title, its rows, and its notices, in that order.
fn card_lines(card: &Card, width: u16) -> Vec<Line<'static>> {
    let measured = RowLayout::measure(&card.rows);
    let tier = measured.tier_for(width);
    let layout = measured.stretched(tier, width);
    let mut lines = vec![title_line(&card.title, width)];
    lines.extend(
        card.rows
            .iter()
            .map(|row| row_line(row, &layout, tier, width)),
    );
    lines.extend(card.notices.iter().map(|notice| {
        Line::from(Span::styled(
            clip(notice, usize::from(width)),
            Style::default().fg(Color::Yellow),
        ))
    }));
    lines
}

/// The title is reversed across the full card width, so it reads as the edge
/// of the card that the removed border used to draw.
fn title_line(title: &str, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let mut text = clip(title, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    text.push_str(&" ".repeat(padding));
    Line::from(Span::styled(
        text,
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    ))
}

/// How much room each field of a card's rows needs, before any is dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowLayout {
    identity: usize,
    verb: usize,
    amount: usize,
    /// The amount, or the verb for a row whose reading is a word such as
    /// `used up`. Tiers that drop the verb column show this instead, so no
    /// row ends up with no reading at all.
    reading: usize,
    suffix: usize,
    resets: usize,
    bar: usize,
}

/// The fields a row keeps at a given width, widest variant first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Identity, reading, reset countdown, progress bar.
    Full,
    /// The bar is the widest field and the only one a percentage repeats.
    NoBar,
    /// The verb is prose; the number it introduces carries the meaning.
    NoVerb,
    /// The countdown goes last, after everything but identity and reading.
    NoReset,
    /// Identity and reading only, both clipped to fit.
    Minimal,
}

impl RowLayout {
    fn measure(rows: &[CardRow]) -> Self {
        Self {
            identity: max_width(rows.iter().map(|row| row.identity.as_str())),
            verb: max_width(rows.iter().map(|row| row.verb)),
            amount: max_width(rows.iter().map(|row| row.amount.as_str())),
            reading: max_width(rows.iter().map(reading)),
            suffix: max_width(rows.iter().map(|row| row.suffix)),
            resets: max_width(rows.iter().filter_map(|row| row.resets.as_deref())),
            bar: if rows.iter().any(|row| row.ratio.is_some()) {
                BAR_RENDER_WIDTH
            } else {
                0
            },
        }
    }

    /// Widens the identity column by the slack left over at `width`, so the
    /// reading, the countdown, and the bar sit against the card's right edge
    /// and line up across the rows of every card.
    fn stretched(mut self, tier: Tier, width: u16) -> Self {
        if tier == Tier::Minimal {
            return self;
        }
        let slack = usize::from(width).saturating_sub(self.width_of(tier));
        self.identity += slack;
        self
    }

    /// The widest tier that fits `width`, or [`Tier::Minimal`] when none does.
    fn tier_for(&self, width: u16) -> Tier {
        let width = usize::from(width);
        for tier in [Tier::Full, Tier::NoBar, Tier::NoVerb, Tier::NoReset] {
            if self.width_of(tier) <= width {
                return tier;
            }
        }
        Tier::Minimal
    }

    fn width_of(&self, tier: Tier) -> usize {
        let fields: &[usize] = match tier {
            Tier::Full => &[
                self.identity,
                self.verb,
                self.amount,
                self.suffix,
                self.resets,
                self.bar,
            ],
            Tier::NoBar => &[
                self.identity,
                self.verb,
                self.amount,
                self.suffix,
                self.resets,
            ],
            Tier::NoVerb => &[self.identity, self.reading, self.suffix, self.resets],
            Tier::NoReset => &[self.identity, self.reading, self.suffix],
            Tier::Minimal => &[self.identity, self.reading],
        };
        let used: usize = fields.iter().filter(|field| **field > 0).sum();
        let gaps = fields.iter().filter(|field| **field > 0).count();
        used + FIELD_GAP * gaps.saturating_sub(1)
    }
}

fn row_line(row: &CardRow, layout: &RowLayout, tier: Tier, width: u16) -> Line<'static> {
    if tier == Tier::Minimal {
        return minimal_line(row, width);
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    push_field(&mut spans, &row.identity, layout.identity, Style::default());
    if matches!(tier, Tier::Full | Tier::NoBar) {
        push_field(&mut spans, row.verb, layout.verb, dim);
        push_field(&mut spans, &row.amount, layout.amount, amount_style(row));
    } else {
        push_field(&mut spans, reading(row), layout.reading, amount_style(row));
    }
    push_field(&mut spans, row.suffix, layout.suffix, dim);
    if matches!(tier, Tier::Full | Tier::NoBar | Tier::NoVerb) {
        push_field(
            &mut spans,
            row.resets.as_deref().unwrap_or(""),
            layout.resets,
            dim,
        );
    }
    if tier == Tier::Full && layout.bar > 0 {
        let bar = row.ratio.map(progress_bar).unwrap_or_default();
        push_field(&mut spans, &bar, layout.bar, amount_style(row));
    }
    Line::from(spans)
}

/// What a row reads as when there is no room for the verb and the amount
/// side by side: the amount, or the verb for `used up`, `unlimited`, and the
/// other readings that are a word rather than a number.
fn reading(row: &CardRow) -> &str {
    if row.amount.is_empty() {
        row.verb
    } else {
        row.amount.as_str()
    }
}

/// The narrowest row keeps the identity and the reading, with the reading
/// kept whole and the identity giving up the cells it needs.
fn minimal_line(row: &CardRow, width: u16) -> Line<'static> {
    let value = reading(row);
    let width = usize::from(width);
    let value = clip(value, width);
    let room = width
        .saturating_sub(UnicodeWidthStr::width(value.as_str()))
        .saturating_sub(FIELD_GAP);
    let identity = clip(&row.identity, room);
    let mut spans = Vec::new();
    if !identity.is_empty() {
        spans.push(Span::raw(identity));
        spans.push(Span::raw(" ".repeat(FIELD_GAP)));
    }
    spans.push(Span::styled(value, amount_style(row)));
    Line::from(spans)
}

fn amount_style(row: &CardRow) -> Style {
    match row.ratio.map(remaining_severity) {
        Some(Severity::Critical) => Style::default().fg(Color::Red),
        Some(Severity::Low) => Style::default().fg(Color::Yellow),
        Some(Severity::Ample) => Style::default().fg(Color::Green),
        None => Style::default(),
    }
}

/// Appends one padded field and the gap that precedes it, skipping the field
/// when the whole column is empty for this card.
fn push_field(spans: &mut Vec<Span<'static>>, text: &str, width: usize, style: Style) {
    if width == 0 {
        return;
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" ".repeat(FIELD_GAP)));
    }
    let text = clip(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    spans.push(Span::styled(text, style));
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
}

/// The hints and the position, shortened as the terminal narrows.
fn status_line(
    offset: u16,
    viewport_height: u16,
    content_height: u16,
    width: u16,
) -> Line<'static> {
    let scrollable = content_height > viewport_height;
    let position = format!("{}/{}", offset.saturating_add(1), content_height);
    let candidates = if scrollable {
        vec![
            format!("q quit  wheel/jk scroll  PgUp/PgDn half page  {position}"),
            format!("q quit  jk  PgUp/PgDn  {position}"),
            format!("q  jk  PgUp/Dn  {position}"),
            format!("q  {position}"),
            "q".to_owned(),
        ]
    } else {
        vec!["q quit".to_owned(), "q".to_owned()]
    };
    let width = usize::from(width);
    let text = candidates
        .into_iter()
        .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= width)
        .unwrap_or_default();
    Line::from(Span::styled(
        clip(&text, width),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// Cuts `text` to `width` display cells, never splitting a wide character.
fn clip(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut used = 0;
    text.chars()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > width {
                false
            } else {
                used += character_width;
                true
            }
        })
        .collect()
}

fn max_width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(UnicodeWidthStr::width).max().unwrap_or(0)
}

#[derive(Debug)]
struct Card {
    title: String,
    rows: Vec<CardRow>,
    notices: Vec<String>,
}

impl Card {
    /// One title line, the rows, and the notices. No border rows.
    fn height(&self) -> u16 {
        (self.rows.len() + self.notices.len() + 1)
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

#[derive(Debug)]
struct CardRow {
    identity: String,
    verb: &'static str,
    amount: String,
    suffix: &'static str,
    resets: Option<String>,
    ratio: Option<f64>,
}

fn cards(snapshots: &[SnapshotPayload], now: DateTime<Utc>) -> Vec<Card> {
    snapshots
        .iter()
        .map(|snapshot| card(snapshot, now))
        .collect()
}

fn card(snapshot: &SnapshotPayload, now: DateTime<Utc>) -> Card {
    let usage = usage_data(&snapshot.usage);
    let summary = MetricFilterChoice::default()
        .for_saved(&snapshot.metrics)
        .summarize(usage);
    let mut notices = Vec::new();
    if snapshot.stale {
        notices.push("! stale snapshot".into());
    }
    if summary.limit_reached {
        notices.push("! limit reached".into());
    }
    if let QueryOutcome::Partial { failures, .. } = &snapshot.usage {
        notices.push(format!("! {} item(s) unavailable", failures.len()));
    }
    if summary.rows.is_empty() {
        notices.push("! no summarized metrics".into());
    }
    Card {
        title: card_title(usage, &snapshot.account_id),
        rows: rows(&summary, now),
        notices,
    }
}

fn card_title(usage: &SubscriptionUsage, account_id: &str) -> String {
    let mut title = sanitize_cell(usage.provider.as_str()).to_owned();
    if let Some(plan) = usage.plan.as_deref().filter(|plan| !plan.trim().is_empty()) {
        title.push('/');
        title.push_str(sanitize_cell(plan));
    }
    title.push_str("  ");
    title.push_str(sanitize_cell(account_id));
    title
}

/// Reuses the table's cells, so the view names windows, metrics, and readings
/// exactly as `show` does, and hides the same rows.
fn rows(summary: &UsageSummary, now: DateTime<Utc>) -> Vec<CardRow> {
    collect_summary_cells(&summary.rows, now)
        .into_iter()
        .map(|cell| {
            let SummaryCells {
                identity,
                verb,
                amount,
                suffix,
                resets_in,
                remaining_ratio,
                ..
            } = cell;
            CardRow {
                identity,
                verb,
                amount,
                suffix,
                resets: resets_in.map(countdown_text),
                ratio: remaining_ratio,
            }
        })
        .collect()
}

fn usage_data(outcome: &QueryOutcome<SubscriptionUsage>) -> &SubscriptionUsage {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
    }
}

fn card_rects(width: u16, heights: &[u16]) -> Vec<Rect> {
    if width == 0 || heights.is_empty() {
        return Vec::new();
    }
    let columns = ((u32::from(width) + u32::from(HORIZONTAL_GAP))
        / u32::from(PREFERRED_CARD_WIDTH + HORIZONTAL_GAP))
    .max(1) as u16;
    let gaps = HORIZONTAL_GAP.saturating_mul(columns.saturating_sub(1));
    let available = width.saturating_sub(gaps);
    let base_width = available / columns;
    let extra = available % columns;
    let mut rects = Vec::with_capacity(heights.len());
    let mut y = 0u16;
    for row in heights.chunks(columns as usize) {
        let row_height = row.iter().copied().max().unwrap_or(0);
        let mut x = 0u16;
        for (column, height) in row.iter().enumerate() {
            let card_width = base_width + u16::from((column as u16) < extra);
            rects.push(Rect::new(x, y, card_width, *height));
            x = x.saturating_add(card_width + HORIZONTAL_GAP);
        }
        y = y.saturating_add(row_height + VERTICAL_GAP);
    }
    rects
}

fn content_height(rects: &[Rect]) -> u16 {
    rects.iter().map(|rect| rect.bottom()).max().unwrap_or(0)
}

trait TerminalControl {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn enable_mouse(&mut self) -> io::Result<()>;
    fn disable_mouse(&mut self) -> io::Result<()>;
    fn hide_cursor(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
}

struct CrosstermControl;

impl TerminalControl for CrosstermControl {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen).map(|_| ())
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture).map(|_| ())
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture).map(|_| ())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), crossterm::cursor::Hide).map(|_| ())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), crossterm::cursor::Show).map(|_| ())
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen).map(|_| ())
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

struct TerminalSession<C: TerminalControl> {
    control: C,
    entered_alternate_screen: bool,
    mouse_captured: bool,
    cursor_hidden: bool,
    raw_enabled: bool,
}

impl<C: TerminalControl> TerminalSession<C> {
    fn enter(mut control: C) -> io::Result<Self> {
        control.enable_raw()?;
        let mut session = Self {
            control,
            entered_alternate_screen: false,
            mouse_captured: false,
            cursor_hidden: false,
            raw_enabled: true,
        };
        session.control.enter_alternate_screen()?;
        session.entered_alternate_screen = true;
        session.control.enable_mouse()?;
        session.mouse_captured = true;
        session.control.hide_cursor()?;
        session.cursor_hidden = true;
        Ok(session)
    }
}

impl<C: TerminalControl> Drop for TerminalSession<C> {
    fn drop(&mut self) {
        if self.cursor_hidden {
            let _ = self.control.show_cursor();
        }
        if self.mouse_captured {
            let _ = self.control.disable_mouse();
        }
        if self.entered_alternate_screen {
            let _ = self.control.leave_alternate_screen();
        }
        if self.raw_enabled {
            let _ = self.control.disable_raw();
        }
    }
}

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
