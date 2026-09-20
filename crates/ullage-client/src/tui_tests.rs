use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::TestBackend;
use ullage_protocol::{ControlResponse, ProviderId, SubscriptionUsage};

use super::cards::*;
use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap()
}

fn row(identity: &str, verb: &'static str, amount: &str, resets: Option<&str>) -> CardRow {
    CardRow {
        identity: identity.into(),
        verb,
        amount: amount.into(),
        suffix: "",
        resets: resets.map(str::to_owned),
        ratio: Some(0.91),
    }
}

fn title() -> CardTitle {
    CardTitle {
        provider: "claude".into(),
        plan: Some("max_20x".into()),
        account: "acct-1".into(),
    }
}

fn card_with_rows(rows: Vec<CardRow>) -> Card {
    Card {
        title: title(),
        rows,
        notices: Vec::new(),
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Queues `action` and draws once, the order the event loop uses.
fn step(scroll: &mut Scroll, action: Action, viewport_height: u16, content_height: u16) -> u16 {
    scroll.pending = action;
    scroll.apply(viewport_height, content_height);
    scroll.offset
}

fn rendered(width: u16, height: u16, cards: &[Card]) -> String {
    rendered_with(width, height, cards, Layout::Columns)
}

fn rendered_with(width: u16, height: u16, cards: &[Card], layout: Layout) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut scroll = Scroll::default();
    terminal
        .draw(|frame| render(frame, cards, &mut scroll, layout, Duration::ZERO))
        .unwrap();
    terminal.backend().to_string()
}

fn snapshot(provider: &str, account_id: &str) -> SnapshotPayload {
    SnapshotPayload {
        account_id: account_id.into(),
        usage: QueryOutcome::Complete {
            data: SubscriptionUsage {
                provider: ProviderId::new(provider),
                account_label: None,
                plan: None,
                subscription_expires_at: None,
                observed_at: now(),
                windows: Vec::new(),
            },
        },
        last_success_at: now(),
        stale: false,
        last_error: None,
        last_error_at: None,
        metrics: Vec::new(),
    }
}

/// Answers every request the way the daemon would, or refuses.
struct StubDaemon {
    answer: fn(&ControlRequest) -> Result<ControlResponse, ClientError>,
}

impl ControlClient for StubDaemon {
    fn send(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        (self.answer)(request)
    }
}

fn snapshots_response(request: &ControlRequest) -> Result<ControlResponse, ClientError> {
    Ok(ControlResponse {
        version: CONTROL_PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        result: ControlResult::Snapshots(vec![snapshot("claude", "personal")]),
        diagnostic: None,
        daemon_version: None,
    })
}

#[test]
fn a_refresh_reads_every_snapshot_again() {
    let client = StubDaemon {
        answer: snapshots_response,
    };

    let snapshots = fetch_snapshots(&client).unwrap();

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].account_id, "personal");
}

#[test]
fn a_refusal_or_a_mismatched_answer_leaves_the_readings_alone() {
    // The caller keeps the snapshots it has when the refresh gives back
    // `None`, so every one of these is a view that simply does not change.
    let unavailable = StubDaemon {
        answer: |_| Err(ClientError::DaemonUnavailable),
    };
    assert!(fetch_snapshots(&unavailable).is_none());

    let wrong_version = StubDaemon {
        answer: |request| {
            let mut response = snapshots_response(request)?;
            response.version = CONTROL_PROTOCOL_VERSION.wrapping_add(1);
            Ok(response)
        },
    };
    assert!(fetch_snapshots(&wrong_version).is_none());

    let wrong_request = StubDaemon {
        answer: |request| {
            let mut response = snapshots_response(request)?;
            response.request_id = "someone else's".into();
            Ok(response)
        },
    };
    assert!(fetch_snapshots(&wrong_request).is_none());

    let wrong_result = StubDaemon {
        answer: |request| {
            let mut response = snapshots_response(request)?;
            response.result = ControlResult::Providers(Vec::new());
            Ok(response)
        },
    };
    assert!(fetch_snapshots(&wrong_result).is_none());
}

#[test]
fn every_card_line_is_exactly_the_card_width() {
    let card = Card {
        title: title(),
        rows: vec![
            row("5h", "remains", "91%", Some("3h05m")),
            row("weekly", "used up", "", Some("◆ 23d ◆")),
        ],
        notices: vec!["! stale snapshot".into()],
    };

    // The box glyphs are East Asian ambiguous, one cell to `unicode-width`:
    // a line that is shorter or wider than the card means a border slipped
    // out of alignment.
    for width in [90u16, 42, 28, 20, 12, 6, 5, 3, 2, 1] {
        for (index, line) in card_lines(
            &card,
            width,
            &RowLayout::measure_all(std::slice::from_ref(&card)),
        )
        .iter()
        .enumerate()
        {
            let text = line_text(line);
            assert_eq!(
                UnicodeWidthStr::width(text.as_str()),
                usize::from(width),
                "width {width} line {index}: {text}"
            );
        }
    }
}

#[test]
fn scrolling_stops_at_the_first_and_last_row() {
    let mut scroll = Scroll::default();

    let mut step = |action| step(&mut scroll, action, 10, 40);

    assert_eq!(step(Action::Up(3)), 0, "already at the top");
    assert_eq!(step(Action::Down(3)), 3);
    assert_eq!(step(Action::HalfDown), 8, "half of a ten-row viewport");
    assert_eq!(step(Action::HalfUp), 3);
    assert_eq!(
        step(Action::Bottom),
        30,
        "content height minus viewport height"
    );
    assert_eq!(step(Action::Down(3)), 30, "already at the bottom");
    assert_eq!(step(Action::Top), 0);
}

#[test]
fn content_shorter_than_the_viewport_does_not_scroll() {
    let mut scroll = Scroll::default();

    assert_eq!(step(&mut scroll, Action::Down(3), 10, 4), 0);
    assert_eq!(step(&mut scroll, Action::Bottom, 10, 4), 0);
}

#[test]
fn a_shrinking_viewport_pulls_the_offset_back() {
    let mut scroll = Scroll {
        offset: 30,
        pending: Action::Ignore,
    };

    scroll.apply(20, 40);

    assert_eq!(scroll.offset, 20);
}

#[test]
fn the_status_line_dates_the_readings() {
    // A refresh that fails changes nothing on screen, so the growing age is
    // how a stalled refresh shows itself.
    let updated = |age| updated_text(Duration::from_secs(age));

    assert_eq!(updated(0), "updated just now");
    assert_eq!(updated(59), "updated just now");
    assert_eq!(updated(60), "updated 1m ago");
    assert_eq!(updated(120), "updated 2m ago");
    assert_eq!(updated(59 * 60), "updated 59m ago");
    assert_eq!(updated(60 * 60), "updated 1h00m ago");
    assert_eq!(updated(150 * 60), "updated 2h30m ago");
}

#[test]
fn the_wheel_scrolls_three_rows_and_other_mouse_events_do_nothing() {
    let wheel = |kind| {
        mouse_action(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    };

    assert_eq!(wheel(MouseEventKind::ScrollUp), Action::Up(WHEEL_LINES));
    assert_eq!(wheel(MouseEventKind::ScrollDown), Action::Down(WHEEL_LINES));
    assert_eq!(
        wheel(MouseEventKind::Down(MouseButton::Left)),
        Action::Ignore
    );
}

#[test]
fn keys_map_to_quitting_paging_and_line_scrolling() {
    let press = |code| key_action(KeyEvent::new(code, KeyModifiers::NONE));

    for code in [KeyCode::Char('q'), KeyCode::Char('Q'), KeyCode::Esc] {
        assert_eq!(press(code), Action::Quit);
    }
    assert_eq!(
        key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Quit
    );
    assert_eq!(press(KeyCode::PageUp), Action::HalfUp);
    assert_eq!(press(KeyCode::PageDown), Action::HalfDown);
    assert_eq!(press(KeyCode::Up), Action::Up(1));
    assert_eq!(press(KeyCode::Char('j')), Action::Down(1));
    assert_eq!(press(KeyCode::Home), Action::Top);
    assert_eq!(press(KeyCode::End), Action::Bottom);
    assert_eq!(press(KeyCode::Char('x')), Action::Ignore);
}

#[test]
fn the_status_line_shortens_with_the_terminal() {
    let line = |width| {
        line_text(&status_line(
            11,
            10,
            40,
            width,
            Layout::Columns,
            Duration::ZERO,
        ))
    };

    assert_eq!(
        line(90),
        "q quit  wheel/jk scroll  PgUp/PgDn half page  v one per row  updated just now  12/40"
    );
    assert_eq!(
        line(62),
        "q quit  jk  PgUp/PgDn  v one per row  updated just now  12/40"
    );
    assert_eq!(line(45), "q quit  jk  PgUp/PgDn  v one per row  12/40");
    assert_eq!(line(24), "q  jk  PgUp/Dn  v  12/40");
    assert_eq!(line(12), "q  v  12/40");
    assert_eq!(line(8), "q  12/40");
    assert_eq!(
        line_text(&status_line(0, 10, 4, 60, Layout::Columns, Duration::ZERO)),
        "q quit  v one per row  updated just now"
    );
    assert_eq!(
        line_text(&status_line(0, 10, 4, 30, Layout::Columns, Duration::ZERO)),
        "q quit  v one per row"
    );
}

#[test]
fn the_layout_hint_names_what_the_key_switches_to() {
    // The hint is what pressing `v` gives you, not what is on screen.
    assert!(
        line_text(&status_line(0, 10, 4, 60, Layout::Vertical, Duration::ZERO))
            .contains("v columns"),
        "{}",
        line_text(&status_line(0, 10, 4, 60, Layout::Vertical, Duration::ZERO))
    );
}

#[test]
fn v_toggles_the_layout_and_the_toggle_leaves_the_offset_alone() {
    assert_eq!(
        key_action(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        Action::ToggleLayout
    );
    assert_eq!(
        key_action(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)),
        Action::ToggleLayout
    );
    assert_eq!(Layout::Columns.toggled(), Layout::Vertical);
    assert_eq!(Layout::Vertical.toggled(), Layout::Columns);

    let mut scroll = Scroll {
        offset: 7,
        pending: Action::ToggleLayout,
    };
    scroll.apply(10, 40);
    assert_eq!(scroll.offset, 7);
}

/// The column the last row's text starts in, and the text itself.
fn status_row(width: u16, layout: Layout, cards: &[Card]) -> (u16, String) {
    let backend = TestBackend::new(width, 9);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut scroll = Scroll::default();
    terminal
        .draw(|frame| render(frame, cards, &mut scroll, layout, Duration::ZERO))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let row = (0..width)
        .map(|x| buffer[(x, 8)].symbol().to_owned())
        .collect::<String>();
    let start = row.chars().take_while(|cell| *cell == ' ').count();
    (start as u16, row.trim().to_owned())
}

#[test]
fn the_hints_sit_under_the_cards() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    // A centered card centers them: the margins on either side of the text
    // match within a cell.
    let (start, text) = status_row(60, Layout::Columns, &cards);
    let end = 60 - start - text.chars().count() as u16;
    assert!(start.abs_diff(end) <= 1, "{start} vs {end}: {text}");

    // Cards that fill the width keep the hints at the left edge.
    let wide = vec![
        card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]),
        card_with_rows(vec![row("weekly", "remains", "40%", Some("3h05m"))]),
    ];
    assert_eq!(status_row(120, Layout::Columns, &wide).0, 0);

    // One card per row is a centered card too, however wide the terminal.
    let (start, text) = status_row(120, Layout::Vertical, &cards);
    let end = 120 - start - text.chars().count() as u16;
    assert!(start.abs_diff(end) <= 1, "{start} vs {end}: {text}");
}

#[test]
fn the_view_reserves_its_last_row_for_the_status_line() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    let content = rendered(42, 4, &cards);

    assert!(content.contains("q quit"), "{content}");
}

#[test]
fn tiny_screen_render_does_not_panic() {
    let cards = vec![card_with_rows(vec![row(
        "long metric",
        "remains",
        "42%",
        Some("3h05m"),
    )])];

    rendered(1, 1, &cards);
}

#[test]
fn terminal_session_restores_in_reverse_order() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    {
        let _session = TerminalSession::enter(RecordingControl(calls.clone())).unwrap();
    }
    assert_eq!(
        *calls.borrow(),
        [
            "raw_on",
            "alt_on",
            "mouse_on",
            "hide",
            "show",
            "mouse_off",
            "alt_off",
            "raw_off"
        ]
    );
}

#[test]
fn failed_terminal_entry_restores_completed_steps() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = TerminalSession::enter(FailingControl(calls.clone()));
    assert!(result.is_err());
    assert_eq!(
        *calls.borrow(),
        [
            "raw_on",
            "alt_on",
            "mouse_on",
            "hide_failed",
            "mouse_off",
            "alt_off",
            "raw_off"
        ]
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
    fn enable_mouse(&mut self) -> io::Result<()> {
        self.record("mouse_on");
        Ok(())
    }
    fn disable_mouse(&mut self) -> io::Result<()> {
        self.record("mouse_off");
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
    fn enable_mouse(&mut self) -> io::Result<()> {
        self.0.borrow_mut().push("mouse_on");
        Ok(())
    }
    fn disable_mouse(&mut self) -> io::Result<()> {
        self.0.borrow_mut().push("mouse_off");
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
