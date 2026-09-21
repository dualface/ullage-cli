//! Full-screen subscription usage view.
//!
//! The view shows the same readings as `show`, in the same words, but packed
//! for a small screen: each subscription is a rounded box with its provider
//! in the top border, and a per-card column layout gives up its widest fields
//! first so the identity, the reading, and the reset countdown survive on a
//! phone. `cards` owns what a card looks like; this file owns the terminal,
//! the keys, and the scroll.

mod cards;
mod preferences;

use std::io;
use std::time::{Duration, Instant};

use chrono::Utc;
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
use ullage_protocol::{CONTROL_PROTOCOL_VERSION, ControlRequest, ControlResult, SnapshotPayload};
use unicode_width::UnicodeWidthStr;

use self::cards::{
    Card, RowLayout, card_lines, card_rects, cards as load_cards, clip, content_height,
    preferred_card_width,
};
use self::preferences::{Layout, Preferences};
use crate::errors::{error_output, result_exit_code, sanitize_partial_failure_controls};
use crate::render::{MetricFilterChoice, RenderView, render_result};
use crate::{
    ClientError, ColorMode, Command, ControlClient, ExitCode, OutputFormat, RunOutput, TuiArgs,
    daemon_upgrade_notice, next_request_id, response_matches_command, to_control_command,
};

/// Rows a single wheel notch scrolls.
const WHEEL_LINES: u16 = 3;
/// How often the view asks the daemon for the snapshots again. The daemon
/// probes each account every five minutes, so this catches every round
/// without polling it for nothing. The whole wait is what the refresh bar
/// in the status line counts down.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// Rows kept blank above the cards, so the view does not start against the
/// first row of the terminal.
const TOP_MARGIN: u16 = 1;
/// Rows kept blank between the cards and the status line.
const STATUS_GAP: u16 = 1;
/// How often the view redraws while it waits for the next refresh, so the
/// countdown moves on its own between refreshes.
const TICK: Duration = Duration::from_secs(1);
/// The countdown as one circle, filled by quarter: an empty circle is the
/// moment the refresh is due, a full one the whole interval. Two of them are
/// East Asian ambiguous, so a CJK locale may draw them wider, as it may the
/// box drawing around them.
const REFRESH_MARKERS: [char; 5] = ['○', '◔', '◑', '◕', '●'];

/// Loads all snapshots, then owns the terminal until the user exits.
pub fn run_tui(client: &dyn ControlClient, args: &TuiArgs) -> RunOutput {
    let command = Command::Tui(args.clone());
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

    match run_terminal(client, snapshots, args.vertical) {
        // The exit code describes the last snapshots the view held, not the
        // ones it opened with.
        Ok(snapshots) => RunOutput {
            stdout: String::new(),
            stderr: upgrade_notice,
            code: snapshots_exit_code(&snapshots),
        },
        Err(_) => error_output(ExitCode::Failure, "tui_failed", OutputFormat::Table),
    }
}

/// Asks the daemon for every snapshot again, or `None` when the answer is
/// unusable. A refresh that fails changes nothing on screen: the view keeps
/// the readings it has and tries again at the next interval.
fn fetch_snapshots(client: &dyn ControlClient) -> Option<Vec<SnapshotPayload>> {
    let command = Command::Tui(TuiArgs::default());
    let request_id = next_request_id();
    let request = ControlRequest::new(&request_id, to_control_command(&command));
    let mut response = client.send(&request).ok()?;
    sanitize_partial_failure_controls(&mut response.result);
    if response.version != CONTROL_PROTOCOL_VERSION
        || response.request_id != request_id
        || response.diagnostic.is_some()
        || !response_matches_command(&command, &response.result)
    {
        return None;
    }
    match response.result {
        ControlResult::Snapshots(snapshots) => Some(snapshots),
        _ => None,
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

fn run_terminal(
    client: &dyn ControlClient,
    mut snapshots: Vec<SnapshotPayload>,
    forced_vertical: bool,
) -> io::Result<Vec<SnapshotPayload>> {
    // `--vertical` forces this run without overwriting what was saved; the
    // `v` key is the only thing that changes the stored layout.
    let mut layout = if forced_vertical {
        Layout::Vertical
    } else {
        preferences::load().layout
    };
    let _session = TerminalSession::enter(CrosstermControl)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut scroll = Scroll::default();
    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    let mut stalled = false;
    loop {
        // The countdowns are rebuilt on every draw so a view left open does
        // not keep showing the wait measured when it was opened.
        let now = Utc::now();
        let cards = load_cards(&snapshots, now);
        let wait = next_refresh.saturating_duration_since(Instant::now());
        // A refresh the daemon could not answer leaves the countdown empty
        // instead of starting it over: the readings have not moved, and a
        // full bar would say they had. The retry still happens on schedule.
        let shown = if stalled { Duration::ZERO } else { wait };
        terminal.draw(|frame| render(frame, &cards, &mut scroll, layout, shown))?;
        // The wait doubles as the refresh timer, and the tick keeps the
        // countdown moving between two refreshes.
        if !event::poll(wait.min(TICK))? {
            if Instant::now() >= next_refresh {
                match fetch_snapshots(client) {
                    Some(fresh) => {
                        snapshots = fresh;
                        stalled = false;
                    }
                    None => stalled = true,
                }
                next_refresh = Instant::now() + REFRESH_INTERVAL;
            }
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) => key_action(key),
            Event::Mouse(mouse) => mouse_action(mouse),
            _ => Action::Ignore,
        };
        match action {
            Action::Quit => return Ok(snapshots),
            Action::ToggleLayout => {
                layout = layout.toggled();
                preferences::save(Preferences { layout });
                // The bands are laid out afresh, so the old offset would
                // point somewhere unrelated.
                scroll = Scroll::default();
            }
            action => scroll.pending = action,
        }
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
    /// Switch between wrapping columns and one card per band.
    ToggleLayout,
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
        KeyCode::Char('v' | 'V') => Action::ToggleLayout,
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
            Action::Quit | Action::ToggleLayout | Action::Ignore => self.offset,
        }
        .min(max_offset);
    }
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    cards: &[Card],
    scroll: &mut Scroll,
    layout: Layout,
    remaining: Duration,
) {
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
    // The last row is the status line, the row above it stays blank, and the
    // first row of the terminal stays blank too; the cards own what is left.
    // The blank rows are outside the viewport, so a scrolled card never
    // reaches the status line, and `chrome` decides how many of them a short
    // view can still afford.
    let (margin, gap, status) = chrome(area);
    let top = area.y + margin;
    let reserved = gap + u16::from(status);
    let viewport = Rect::new(
        area.x,
        top,
        area.width,
        area.height.saturating_sub(margin + reserved),
    );
    // One set of column widths for the whole screen, so a reading under a
    // short window name still lines up with the one under a long name. The
    // cards are then sized to hold those columns.
    let columns = RowLayout::measure_all(cards);
    let heights = cards.iter().map(Card::height).collect::<Vec<_>>();
    let rects = card_rects(
        area.width,
        &heights,
        layout.is_vertical(),
        preferred_card_width(&columns),
    );
    let content = content_height(&rects);
    scroll.apply(viewport.height, content);
    for (card, rect) in cards.iter().zip(rects) {
        render_card(frame, card, rect, viewport, scroll.offset, &columns);
    }
    // The hints sit under the cards, centered on the terminal whatever the
    // cards do: a centered band and a full-width row put them in the same
    // place, which is what makes the line stop jumping as the layout
    // changes.
    if status {
        let status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let line = status_line(
            scroll.offset,
            viewport.height,
            content,
            area.width,
            layout,
            remaining,
        );
        let text_width = u16::try_from(line_width(&line)).unwrap_or(area.width);
        let x = area.x + area.width.saturating_sub(text_width) / 2;
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(x, status.y, status.right().saturating_sub(x), 1),
        );
    }
}

/// The blank rows above the cards and between them and the status line, plus
/// whether the status line fits at all, for a terminal `area.height` rows
/// tall. Each blank row and the status line cost a row, and the cards keep at
/// least one: a short view spends its rows on the cards and the hints before
/// it spends them on padding, giving up the top margin, then the gap, and the
/// status line last.
fn chrome(area: Rect) -> (u16, u16, bool) {
    let mut margin = TOP_MARGIN.min(area.height.saturating_sub(1));
    let mut gap = STATUS_GAP;
    let mut status = area.height >= 2;
    while status && area.height < margin + gap + 2 {
        if margin > 0 {
            margin -= 1;
        } else if gap > 0 {
            gap -= 1;
        } else {
            status = false;
        }
    }
    if !status {
        gap = 0;
    }
    (margin, gap, status)
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Draws the lines of one card that the scrolled viewport still shows.
fn render_card(
    frame: &mut ratatui::Frame<'_>,
    card: &Card,
    rect: Rect,
    viewport: Rect,
    offset: u16,
    columns: &RowLayout,
) {
    if rect.width == 0 {
        return;
    }
    for (index, line) in card_lines(card, rect.width, columns)
        .into_iter()
        .enumerate()
    {
        let row = i64::from(rect.y) + index as i64 - i64::from(offset);
        if row < 0 || row >= i64::from(viewport.height) {
            continue;
        }
        let y = viewport.y + row as u16;
        frame.render_widget(Paragraph::new(line), Rect::new(rect.x, y, rect.width, 1));
    }
}

/// The hints, the countdown to the next refresh, and the position,
/// shortened as the terminal narrows.
fn status_line(
    offset: u16,
    viewport_height: u16,
    content_height: u16,
    width: u16,
    layout: Layout,
    remaining: Duration,
) -> Line<'static> {
    let scrollable = content_height > viewport_height;
    let position = format!("{}/{}", offset.saturating_add(1), content_height);
    // How long until the view reads the snapshots again, which is also how a
    // refresh in progress shows: the circle empties and waits there.
    let marker = refresh_marker(remaining);
    let countdown = format!("{marker} {}", remaining_text(remaining));
    // The key says what pressing it gives you, not what you are looking at.
    let layout_hint = if layout.is_vertical() {
        "v columns"
    } else {
        "v one per row"
    };
    // Each candidate drops something the one above it kept, so a narrower
    // terminal never shows more than a wider one: first the wait in words,
    // then the position, then the layout hint, and the circle last.
    let candidates = if scrollable {
        vec![
            format!(
                "q quit  wheel/jk scroll  PgUp/PgDn half page  {layout_hint}  {countdown}  {position}"
            ),
            format!("q quit  jk  PgUp/PgDn  {layout_hint}  {countdown}  {position}"),
            format!("q quit  jk  PgUp/PgDn  {layout_hint}  {marker}  {position}"),
            format!("q quit  jk  PgUp/PgDn  {layout_hint}  {marker}"),
            format!("q  jk  PgUp/Dn  v  {marker}"),
            format!("q  v  {marker}"),
            format!("q  {marker}"),
            "q".to_owned(),
        ]
    } else {
        vec![
            format!("q quit  {layout_hint}  {countdown}"),
            format!("q quit  {layout_hint}  {marker}"),
            format!("q quit  {marker}"),
            format!("q  {marker}"),
            "q".to_owned(),
        ]
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

/// The wait to the next refresh as one circle filled by quarter: the whole
/// interval is a full circle, the moment the refresh is due is an empty one,
/// and each quarter is a quarter of the wait. Rounded up, so any wait at all
/// still shows and the circle empties exactly when the refresh is due.
fn refresh_marker(remaining: Duration) -> char {
    let interval = REFRESH_INTERVAL.as_secs();
    let left = remaining.as_secs().min(interval);
    let quarters = (left * 4).div_ceil(interval);
    REFRESH_MARKERS[quarters as usize]
}

/// `2m00s`: the wait until the next refresh, in whole seconds. The minutes
/// are always there so the line keeps its width as the countdown runs.
fn remaining_text(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    format!("{}m{:02}s", seconds / 60, seconds % 60)
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
