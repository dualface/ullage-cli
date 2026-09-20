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

use self::cards::{Card, card_lines, card_rects, cards as load_cards, clip, content_height};
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
/// without polling it for nothing.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60);

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
    let mut loaded_at = Utc::now();
    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    loop {
        // The countdowns are rebuilt on every draw so a view left open does
        // not keep showing the wait measured when it was opened.
        let now = Utc::now();
        let cards = load_cards(&snapshots, now);
        let age = (now - loaded_at).to_std().unwrap_or_default();
        terminal.draw(|frame| render(frame, &cards, &mut scroll, layout, age))?;
        // The wait doubles as the refresh timer: an idle view still redraws
        // every interval, so the countdowns keep moving on their own.
        if !event::poll(next_refresh.saturating_duration_since(Instant::now()))? {
            if let Some(fresh) = fetch_snapshots(client) {
                snapshots = fresh;
                loaded_at = Utc::now();
            }
            next_refresh = Instant::now() + REFRESH_INTERVAL;
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
    age: Duration,
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
    // The status line only earns its row once there are two.
    let (viewport, status) = match area.height {
        1 => (area, None),
        _ => (
            Rect::new(area.x, area.y, area.width, area.height - 1),
            Some(Rect::new(area.x, area.bottom() - 1, area.width, 1)),
        ),
    };
    let heights = cards.iter().map(Card::height).collect::<Vec<_>>();
    let rects = card_rects(area.width, &heights, layout.is_vertical());
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
                layout,
                age,
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

/// The hints and the position, shortened as the terminal narrows.
fn status_line(
    offset: u16,
    viewport_height: u16,
    content_height: u16,
    width: u16,
    layout: Layout,
    age: Duration,
) -> Line<'static> {
    let scrollable = content_height > viewport_height;
    let position = format!("{}/{}", offset.saturating_add(1), content_height);
    // How old the readings are, which is also how a failed refresh shows:
    // the age keeps growing past the interval.
    let updated = updated_text(age);
    // The key says what pressing it gives you, not what you are looking at.
    let layout_hint = if layout.is_vertical() {
        "v columns"
    } else {
        "v one per row"
    };
    let candidates = if scrollable {
        vec![
            format!(
                "q quit  wheel/jk scroll  PgUp/PgDn half page  {layout_hint}  {updated}  {position}"
            ),
            format!("q quit  jk  PgUp/PgDn  {layout_hint}  {updated}  {position}"),
            format!("q quit  jk  PgUp/PgDn  {layout_hint}  {position}"),
            format!("q  jk  PgUp/Dn  v  {position}"),
            format!("q  v  {position}"),
            format!("q  {position}"),
            "q".to_owned(),
        ]
    } else {
        vec![
            format!("q quit  {layout_hint}  {updated}"),
            format!("q quit  {layout_hint}"),
            "q  v".to_owned(),
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

/// `updated 2m ago`, in the same words the table uses for a snapshot's age.
fn updated_text(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        return "updated just now".to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("updated {minutes}m ago");
    }
    format!("updated {}h{:02}m ago", minutes / 60, minutes % 60)
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
