use std::cell::RefCell;
use std::rc::Rc;

use chrono::{TimeZone, Utc};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::TestBackend;
use ullage_core::summary::{SummaryRow, SummaryValue, UsageSummary};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap()
}

fn summary(rows: Vec<SummaryRow>) -> UsageSummary {
    UsageSummary {
        rows,
        limit_reached: false,
        observed_at: now(),
        expires_at: None,
    }
}

fn usage_row(window: &str, percent: f64, resets_in_seconds: i64) -> SummaryRow {
    SummaryRow {
        window: window.into(),
        metric: "usage".into(),
        value: SummaryValue::Remains(percent),
        resets_at: Some(now() + chrono::Duration::seconds(resets_in_seconds)),
        remaining_ratio: Some(percent / 100.0),
        disabled: false,
    }
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

fn card_with_rows(rows: Vec<CardRow>) -> Card {
    Card {
        title: "claude/max_20x  acct-1".into(),
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
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut scroll = Scroll::default();
    terminal
        .draw(|frame| render(frame, cards, &mut scroll))
        .unwrap();
    terminal.backend().to_string()
}

#[test]
fn rows_reuse_the_table_identity_reading_and_row_filter() {
    let rows = rows(
        &summary(vec![
            usage_row("5h", 91.0, 3 * 3_600 + 300),
            SummaryRow {
                window: "weekly".into(),
                metric: "on demand".into(),
                value: SummaryValue::Disabled,
                resets_at: None,
                remaining_ratio: None,
                disabled: true,
            },
        ]),
        now(),
    );

    // `5h usage` collapses to `5h`, and the disabled on-demand row is hidden
    // exactly as `show` hides it.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].identity, "5h");
    assert_eq!(rows[0].verb, "remains");
    assert_eq!(rows[0].amount, "91%");
    assert_eq!(rows[0].resets.as_deref(), Some("3h05m"));
}

#[test]
fn disabled_rows_keep_the_off_suffix() {
    let rows = rows(
        &summary(vec![SummaryRow {
            window: "weekly".into(),
            metric: "usage".into(),
            value: SummaryValue::Remains(40.0),
            resets_at: None,
            remaining_ratio: Some(0.4),
            disabled: true,
        }]),
        now(),
    );

    assert_eq!(rows[0].suffix, "(off)");
}

#[test]
fn countdowns_cover_days_hours_minutes_and_expiry() {
    let cases = [
        (2 * 86_400 + 4 * 3_600, "2d04h"),
        (3 * 3_600 + 300, "3h05m"),
        (12 * 60, "12m"),
        (30, "<1m"),
        (-3_600, "<1m"),
    ];
    for (seconds, expected) in cases {
        let rows = rows(&summary(vec![usage_row("5h", 91.0, seconds)]), now());
        assert_eq!(rows[0].resets.as_deref(), Some(expected), "{seconds}s");
    }
}

#[test]
fn a_card_costs_one_line_beyond_its_rows_and_notices() {
    let card = Card {
        title: "claude/max_20x  acct-1".into(),
        rows: vec![row("5h", "remains", "91%", Some("3h05m"))],
        notices: vec!["! stale snapshot".into()],
    };

    assert_eq!(card.height(), 3);
    assert_eq!(card_lines(&card, 42).len(), 3);
}

#[test]
fn a_full_row_uses_every_column_of_the_card() {
    let card = card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]);

    let line = &card_lines(&card, 42)[1];
    let text = line_text(line);

    // No border eats a column: the row spans the card and the bar ends on
    // its last cell.
    assert_eq!(UnicodeWidthStr::width(text.as_str()), 42, "{text}");
    assert!(text.ends_with(']'), "{text}");
    assert!(text.starts_with("5h "), "{text}");
    assert!(text.contains("remains 91% 3h05m ["), "{text}");
}

#[test]
fn narrowing_drops_the_bar_then_the_verb_then_the_countdown() {
    let card = card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]);
    let layout = RowLayout::measure(&card.rows);

    let tiers = [
        (42, Tier::Full, "5h remains 91% 3h05m [-#########]"),
        (24, Tier::NoBar, "5h remains 91% 3h05m"),
        (16, Tier::NoVerb, "5h 91% 3h05m"),
        (9, Tier::NoReset, "5h 91%"),
        (5, Tier::Minimal, "5 91%"),
    ];
    for (width, tier, expected) in tiers {
        assert_eq!(layout.tier_for(width), tier, "width {width}");
        let text = line_text(&row_line(&card.rows[0], &layout, tier, width));
        assert_eq!(text.trim_end(), expected, "width {width}");
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= usize::from(width) || tier == Tier::Minimal,
            "width {width}: {text}"
        );
    }
}

#[test]
fn the_narrowest_row_keeps_a_wordy_reading_when_there_is_no_amount() {
    let used_up = CardRow {
        identity: "weekly".into(),
        verb: "used up",
        amount: String::new(),
        suffix: "",
        resets: Some("6d21h".into()),
        ratio: Some(0.0),
    };
    let layout = RowLayout::measure(std::slice::from_ref(&used_up));

    let text = line_text(&row_line(&used_up, &layout, Tier::Minimal, 12));

    assert_eq!(text, "week used up");
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
    assert_eq!(
        line_text(&status_line(11, 10, 40, 60)),
        "q quit  wheel/jk scroll  PgUp/PgDn half page  12/40"
    );
    assert_eq!(
        line_text(&status_line(11, 10, 40, 30)),
        "q quit  jk  PgUp/PgDn  12/40"
    );
    assert_eq!(
        line_text(&status_line(11, 10, 40, 21)),
        "q  jk  PgUp/Dn  12/40"
    );
    assert_eq!(line_text(&status_line(11, 10, 40, 8)), "q  12/40");
    assert_eq!(line_text(&status_line(0, 10, 4, 60)), "q quit");
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
fn cards_flow_left_to_right_then_wrap() {
    let rects = card_rects(86, &[5, 4, 6]);
    assert_eq!(rects[0], Rect::new(0, 0, 42, 5));
    assert_eq!(rects[1], Rect::new(44, 0, 42, 4));
    assert_eq!(
        rects[2],
        Rect::new(0, 5, 42, 6),
        "no blank row between rows"
    );
}

#[test]
fn one_cell_short_of_two_cards_wraps_to_one_column() {
    let rects = card_rects(85, &[3, 3]);
    assert_eq!(rects, [Rect::new(0, 0, 85, 3), Rect::new(0, 3, 85, 3)]);
}

#[test]
fn narrow_screen_compresses_card_to_available_width() {
    assert_eq!(card_rects(12, &[4]), [Rect::new(0, 0, 12, 4)]);
}

#[test]
fn content_height_is_the_lowest_card_bottom() {
    assert_eq!(content_height(&card_rects(86, &[5, 4, 6])), 11);
    assert_eq!(content_height(&[]), 0);
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
