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
fn the_refresh_bar_empties_over_the_interval() {
    let bar = |seconds| refresh_bar(Duration::from_secs(seconds), REFRESH_BAR_CELLS);

    // A fresh load starts full and the refresh is due at an empty bar; a wait
    // longer than the interval (which the caller never produces) is clamped
    // to full rather than overflowing.
    assert_eq!(bar(120), "██████████");
    assert_eq!(bar(300), "██████████");
    assert_eq!(bar(0), "░░░░░░░░░░");
    // Ten cells over two minutes: one cell every twelve seconds, and the cell
    // the countdown is inside moves in eighths, so it takes about a second
    // and a half for the bar to change again.
    assert_eq!(bar(119), "██████████");
    assert_eq!(bar(118), "█████████▉");
    assert_eq!(bar(114), "█████████▌");
    assert_eq!(bar(108), "█████████░");
    assert_eq!(bar(12), "█░░░░░░░░░");
    assert_eq!(bar(6), "▌░░░░░░░░░");
    assert_eq!(bar(1), "▏░░░░░░░░░");
}

#[test]
fn the_refresh_bar_keeps_its_width_and_shrinks_on_a_narrow_terminal() {
    for seconds in [0u64, 1, 7, 12, 60, 119, 120] {
        for cells in [REFRESH_BAR_CELLS, MINI_REFRESH_BAR_CELLS] {
            let bar = refresh_bar(Duration::from_secs(seconds), cells);
            assert_eq!(
                UnicodeWidthStr::width(bar.as_str()),
                cells,
                "{seconds}s, {cells} cells: {bar}"
            );
        }
    }
}

#[test]
fn the_countdown_reads_as_whole_seconds() {
    let text = |seconds| remaining_text(Duration::from_secs(seconds));

    assert_eq!(text(120), "2m00s");
    assert_eq!(text(119), "1m59s");
    assert_eq!(text(45), "0m45s");
    assert_eq!(text(0), "0m00s");
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
            Duration::from_secs(120),
        ))
    };

    assert_eq!(
        line(90),
        "q quit  wheel/jk scroll  PgUp/PgDn half page  v one per row  ██████████ 2m00s  12/40"
    );
    assert_eq!(
        line(61),
        "q quit  jk  PgUp/PgDn  v one per row  ██████████ 2m00s  12/40"
    );
    assert_eq!(
        line(55),
        "q quit  jk  PgUp/PgDn  v one per row  ██████████  12/40"
    );
    assert_eq!(
        line(49),
        "q quit  jk  PgUp/PgDn  v one per row  ████  12/40"
    );
    assert_eq!(line(30), "q  jk  PgUp/Dn  v  ████  12/40");
    assert_eq!(line(17), "q  v  ████  12/40");
    assert_eq!(line(8), "q  12/40");
    assert_eq!(line(1), "q");
    assert_eq!(
        line_text(&status_line(
            0,
            10,
            4,
            60,
            Layout::Columns,
            Duration::from_secs(120)
        )),
        "q quit  v one per row  ██████████ 2m00s"
    );
    assert_eq!(
        line_text(&status_line(
            0,
            10,
            4,
            38,
            Layout::Columns,
            Duration::from_secs(120)
        )),
        "q quit  v one per row  ██████████"
    );
    assert_eq!(
        line_text(&status_line(
            0,
            10,
            4,
            30,
            Layout::Columns,
            Duration::from_secs(120)
        )),
        "q quit  v one per row  ████"
    );
    assert_eq!(
        line_text(&status_line(
            0,
            10,
            4,
            22,
            Layout::Columns,
            Duration::from_secs(120)
        )),
        "q quit  v one per row"
    );
    assert_eq!(
        line_text(&status_line(
            0,
            10,
            4,
            1,
            Layout::Columns,
            Duration::from_secs(120)
        )),
        "q"
    );
    // Each step only ever drops a piece: a narrow terminal never shows
    // something a wider one had already given up, so every piece is present
    // on a run of widths that ends at the widest.
    type Present = fn(&str) -> bool;
    let features: [(&str, Present); 4] = [
        ("the wait in words", |text| text.contains("2m00s")),
        ("the ten-cell bar", |text| text.contains("██████████")),
        ("the four-cell bar", |text| {
            !text.contains("██████████") && text.contains("████")
        }),
        ("the position", |text| text.contains("12/40")),
    ];
    for (name, present) in features {
        let widths: Vec<u16> = (1..=90).filter(|width| present(&line(*width))).collect();
        let contiguous = widths.windows(2).all(|pair| pair[1] == pair[0] + 1);
        assert!(!widths.is_empty(), "{name} never appears");
        assert!(
            contiguous,
            "{name} comes and goes across widths: {widths:?}"
        );
    }
    // And the order they go in is the order they are dropped: the wait in
    // words first, then the position, the ten-cell bar, and the four-cell one
    // in place of it.
    assert!(
        line(90).contains("2m00s")
            && line(60).contains("██████████")
            && !line(30).contains("2m00s"),
        "{}",
        line(30)
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

/// The column the status line's text starts in, and the text itself.
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

/// Every row of the rendered view, with the trailing blanks kept.
fn rendered_rows(width: u16, height: u16, cards: &[Card]) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut scroll = Scroll::default();
    terminal
        .draw(|frame| render(frame, cards, &mut scroll, Layout::Columns, Duration::ZERO))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn the_view_keeps_a_blank_row_above_the_cards() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    let rows = rendered_rows(42, 6, &cards);

    assert_eq!(rows[0].trim(), "", "the first row is blank: {rows:?}");
    assert!(
        rows[1].contains('╭'),
        "the card starts on the second: {rows:?}"
    );
}

#[test]
fn a_blank_row_separates_the_cards_from_the_status_line() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    // The card is three rows tall and the view is six: the last row is the
    // status line and the row above it stays empty.
    let rows = rendered_rows(42, 6, &cards);

    assert!(rows[3].contains('╰'), "the card ends here: {rows:?}");
    assert_eq!(rows[4].trim(), "", "the gap row: {rows:?}");
    assert!(rows[5].contains('q'), "the status line: {rows:?}");
}

#[test]
fn a_scrolled_card_stops_at_the_gap_row() {
    // Content taller than the viewport: the rows left over are dropped, never
    // drawn over the gap row or the status line.
    let cards = vec![card_with_rows(vec![
        row("5h", "remains", "91%", Some("3h05m")),
        row("weekly", "remains", "40%", Some("3h05m")),
    ])];

    let rows = rendered_rows(42, 6, &cards);

    assert_eq!(rows[4].trim(), "", "the gap row: {rows:?}");
    assert!(rows[5].contains('q'), "the status line: {rows:?}");
    assert!(
        !rows[4].contains('│') && !rows[5].contains('│'),
        "no card row reaches the last two: {rows:?}"
    );
}

#[test]
fn the_hints_sit_in_the_middle_however_wide_the_cards_are() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    // A centered card, and cards that fill the width: both leave the same
    // margins, so the line does not jump as the layout changes.
    let (centered_start, centered_text) = status_row(60, Layout::Columns, &cards);
    let centered_end = 60 - centered_start - centered_text.chars().count() as u16;
    assert!(
        centered_start.abs_diff(centered_end) <= 1,
        "{centered_start} vs {centered_end}: {centered_text}"
    );

    let wide = vec![
        card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]),
        card_with_rows(vec![row("weekly", "remains", "40%", Some("3h05m"))]),
    ];
    let (full_start, full_text) = status_row(120, Layout::Columns, &wide);
    let full_end = 120 - full_start - full_text.chars().count() as u16;
    assert!(
        full_start.abs_diff(full_end) <= 1,
        "{full_start} vs {full_end}: {full_text}"
    );

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

    // Four rows is the smallest view that still has a status line: one blank
    // row, one row of card, the gap row, and the status line. Narrow enough
    // that only the shortest hint fits, so the row is the hints and nothing
    // else.
    let rows = rendered_rows(42, 4, &cards);

    assert!(rows[3].contains("q  jk"), "the status line: {rows:?}");
    assert!(rows[3].contains("1/3"), "the position: {rows:?}");
    assert_eq!(rows[2].trim(), "", "the gap row: {rows:?}");
}

#[test]
fn a_view_too_short_for_the_status_line_shows_the_card_instead() {
    let cards = vec![card_with_rows(vec![row(
        "5h",
        "remains",
        "91%",
        Some("3h05m"),
    )])];

    // Two rows have room for the top margin and one row of card, and no room
    // for the gap row or the status line.
    let rows = rendered_rows(42, 2, &cards);

    assert_eq!(rows[0].trim(), "", "the first row is blank: {rows:?}");
    assert!(rows[1].contains('╭'), "the card keeps this row: {rows:?}");
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
