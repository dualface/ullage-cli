//! Full-screen subscription usage view.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ullage_core::QueryOutcome;
use ullage_core::summary::{SummaryValue, UsageSummary};
use ullage_protocol::{
    CONTROL_PROTOCOL_VERSION, ControlRequest, ControlResult, SnapshotPayload, SubscriptionUsage,
};

use crate::errors::{error_output, result_exit_code, sanitize_partial_failure_controls};
use crate::render::{MetricFilterChoice, RenderView, render_result};
use crate::{
    ClientError, ColorMode, Command, ControlClient, ExitCode, OutputFormat, RunOutput,
    daemon_upgrade_notice, next_request_id, response_matches_command, sanitize_cell,
    to_control_command,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const PREFERRED_CARD_WIDTH: u16 = 42;
const HORIZONTAL_GAP: u16 = 1;
const VERTICAL_GAP: u16 = 1;
const GRAPH_MIN_WIDTH: u16 = 22;
const GRAPH_WIDTH: u16 = 12;

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
    let cards = snapshots.iter().map(Card::from).collect::<Vec<_>>();
    let _session = TerminalSession::enter(CrosstermControl)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    loop {
        terminal.draw(|frame| render(frame, &cards))?;
        if let Event::Key(key) = event::read()?
            && should_exit(key)
        {
            return Ok(());
        }
    }
}

fn should_exit(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char('q' | 'Q') | KeyCode::Esc)
}

fn render(frame: &mut ratatui::Frame<'_>, cards: &[Card]) {
    let area = frame.area();
    if cards.is_empty() {
        frame.render_widget(
            Paragraph::new("No subscription snapshots").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    let heights = cards.iter().map(Card::height).collect::<Vec<_>>();
    for (card, rect) in cards.iter().zip(card_rects(area.width, &heights)) {
        if let Some(visible) = intersect(rect, area) {
            render_card(frame, card, visible);
        }
    }
}

fn render_card(frame: &mut ratatui::Frame<'_>, card: &Card, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(card.title.clone()).style(Style::default().add_modifier(Modifier::BOLD)))
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    for row in &card.rows {
        if y >= inner.bottom() {
            return;
        }
        render_row(frame, row, Rect::new(inner.x, y, inner.width, 1));
        y = y.saturating_add(1);
    }
    for notice in &card.notices {
        if y >= inner.bottom() {
            return;
        }
        frame.render_widget(
            Paragraph::new(notice.as_str()).style(Style::default().fg(Color::Yellow)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
}

fn render_row(frame: &mut ratatui::Frame<'_>, row: &CardRow, area: Rect) {
    let percent = row.ratio.map(|ratio| (ratio * 100.0).round() as u16);
    if area.width >= GRAPH_MIN_WIDTH
        && let Some(percent) = percent
    {
        let gauge_width = GRAPH_WIDTH.min(area.width);
        let label_width = area.width.saturating_sub(gauge_width + 1);
        frame.render_widget(
            Paragraph::new(row.label.as_str()),
            Rect::new(area.x, area.y, label_width, 1),
        );
        frame.render_widget(
            Gauge::default()
                .ratio(f64::from(percent) / 100.0)
                .label(format!("{percent}%"))
                .gauge_style(progress_style(percent)),
            Rect::new(
                area.x.saturating_add(label_width + 1),
                area.y,
                gauge_width,
                1,
            ),
        );
    } else {
        let text = match percent {
            Some(percent) => compact_progress(&row.label, percent, area.width),
            None => format!("{} {}", row.label, row.value),
        };
        frame.render_widget(Paragraph::new(text), area);
    }
}

fn compact_progress(label: &str, percent: u16, width: u16) -> String {
    let percent = format!("{percent}%");
    let percent_width = UnicodeWidthStr::width(percent.as_str());
    let width = usize::from(width);
    if width <= percent_width {
        return percent;
    }
    let label_width = width - percent_width - 1;
    let mut used = 0;
    let label = label
        .chars()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > label_width {
                false
            } else {
                used += character_width;
                true
            }
        })
        .collect::<String>();
    if label.is_empty() {
        percent
    } else {
        format!("{label} {percent}")
    }
}

fn progress_style(percent: u16) -> Style {
    let color = match percent {
        0..=10 => Color::Red,
        11..=25 => Color::Yellow,
        _ => Color::Green,
    };
    Style::default().fg(color)
}

#[derive(Debug)]
struct Card {
    title: String,
    rows: Vec<CardRow>,
    notices: Vec<String>,
}

impl Card {
    fn height(&self) -> u16 {
        (self.rows.len() + self.notices.len() + 2)
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

impl From<&SnapshotPayload> for Card {
    fn from(snapshot: &SnapshotPayload) -> Self {
        let usage = usage_data(&snapshot.usage);
        let mut title = sanitize_cell(usage.provider.as_str()).to_owned();
        if let Some(plan) = usage.plan.as_deref().filter(|plan| !plan.trim().is_empty()) {
            title.push_str(" - ");
            title.push_str(sanitize_cell(plan));
        }
        title.push_str(" - ");
        title.push_str(sanitize_cell(&snapshot.account_id));

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
        Self {
            title,
            rows: rows(&summary),
            notices,
        }
    }
}

#[derive(Debug)]
struct CardRow {
    label: String,
    value: String,
    ratio: Option<f64>,
}

fn rows(summary: &UsageSummary) -> Vec<CardRow> {
    summary
        .rows
        .iter()
        .map(|row| CardRow {
            label: sanitize_cell(&format!("{} {}", row.window, row.metric)).to_owned(),
            value: value_text(&row.value),
            ratio: row.remaining_ratio,
        })
        .collect()
}

fn value_text(value: &SummaryValue) -> String {
    match value {
        SummaryValue::Remains(percent) => format!("{percent:.0}% left"),
        SummaryValue::Used(percent) => format!("{percent:.0}% used"),
        SummaryValue::Balance { amount, currency } => format!("{amount:.2} {}", currency.code),
        SummaryValue::Spent {
            amount,
            limit,
            currency,
        } => format!("{amount:.2}/{limit:.2} {}", currency.code),
        SummaryValue::Credits { used, limit } | SummaryValue::Counted { used, limit } => limit
            .map(|limit| format!("{used:.0}/{limit:.0}"))
            .unwrap_or_else(|| format!("{used:.0}")),
        SummaryValue::CreditsUnlimited => "unlimited".into(),
        SummaryValue::Disabled => "disabled".into(),
    }
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

fn intersect(rect: Rect, viewport: Rect) -> Option<Rect> {
    let left = rect.x.max(viewport.x);
    let top = rect.y.max(viewport.y);
    let right = rect.right().min(viewport.right());
    let bottom = rect.bottom().min(viewport.bottom());
    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
}

trait TerminalControl {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
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
    cursor_hidden: bool,
    raw_enabled: bool,
}

impl<C: TerminalControl> TerminalSession<C> {
    fn enter(mut control: C) -> io::Result<Self> {
        control.enable_raw()?;
        let mut session = Self {
            control,
            entered_alternate_screen: false,
            cursor_hidden: false,
            raw_enabled: true,
        };
        session.control.enter_alternate_screen()?;
        session.entered_alternate_screen = true;
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
        if self.entered_alternate_screen {
            let _ = self.control.leave_alternate_screen();
        }
        if self.raw_enabled {
            let _ = self.control.disable_raw();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn cards_flow_left_to_right_then_wrap() {
        let rects = card_rects(85, &[5, 4, 6]);
        assert_eq!(rects[0], Rect::new(0, 0, 42, 5));
        assert_eq!(rects[1], Rect::new(43, 0, 42, 4));
        assert_eq!(rects[2], Rect::new(0, 6, 42, 6));
    }

    #[test]
    fn one_cell_short_of_two_cards_wraps_to_one_column() {
        let rects = card_rects(84, &[3, 3]);
        assert_eq!(rects, [Rect::new(0, 0, 84, 3), Rect::new(0, 4, 84, 3)]);
    }

    #[test]
    fn narrow_screen_compresses_card_to_available_width() {
        assert_eq!(card_rects(12, &[4]), [Rect::new(0, 0, 12, 4)]);
    }

    #[test]
    fn tiny_screen_render_does_not_panic() {
        let backend = TestBackend::new(1, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let cards = vec![Card {
            title: "long title".into(),
            rows: vec![CardRow {
                label: "long metric".into(),
                value: "value".into(),
                ratio: Some(0.42),
            }],
            notices: Vec::new(),
        }];
        terminal.draw(|frame| render(frame, &cards)).unwrap();
    }

    #[test]
    fn narrow_row_keeps_percentage_without_graph() {
        let backend = TestBackend::new(8, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_row(
                    frame,
                    &CardRow {
                        label: "usage".into(),
                        value: "42% left".into(),
                        ratio: Some(0.42),
                    },
                    frame.area(),
                )
            })
            .unwrap();
        let content = terminal.backend().to_string();
        assert!(content.contains("42%"), "{content}");
        assert!(!content.contains('█'), "{content}");
    }

    #[test]
    fn q_upper_q_and_escape_exit() {
        for code in [KeyCode::Char('q'), KeyCode::Char('Q'), KeyCode::Esc] {
            assert!(should_exit(KeyEvent::new(code, KeyModifiers::NONE)));
        }
        assert!(!should_exit(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn terminal_session_restores_in_reverse_order() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        {
            let _session = TerminalSession::enter(RecordingControl(calls.clone())).unwrap();
        }
        assert_eq!(
            *calls.borrow(),
            ["raw_on", "alt_on", "hide", "show", "alt_off", "raw_off"]
        );
    }

    #[test]
    fn failed_terminal_entry_restores_completed_steps() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let result = TerminalSession::enter(FailingControl(calls.clone()));
        assert!(result.is_err());
        assert_eq!(
            *calls.borrow(),
            ["raw_on", "alt_on", "hide_failed", "alt_off", "raw_off"]
        );
    }

    struct RecordingControl(Rc<RefCell<Vec<&'static str>>>);

    struct FailingControl(Rc<RefCell<Vec<&'static str>>>);

    impl RecordingControl {
        fn record(&self, call: &'static str) {
            self.0.borrow_mut().push(call);
        }
    }

    impl TerminalControl for RecordingControl {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.record("raw_on");
            Ok(())
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alt_on");
            Ok(())
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.record("hide");
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.record("show");
            Ok(())
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alt_off");
            Ok(())
        }
        fn disable_raw(&mut self) -> io::Result<()> {
            self.record("raw_off");
            Ok(())
        }
    }

    impl TerminalControl for FailingControl {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("raw_on");
            Ok(())
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("alt_on");
            Ok(())
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("hide_failed");
            Err(io::Error::other("hide failed"))
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("show");
            Ok(())
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("alt_off");
            Ok(())
        }
        fn disable_raw(&mut self) -> io::Result<()> {
            self.0.borrow_mut().push("raw_off");
            Ok(())
        }
    }
}
