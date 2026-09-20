use std::cell::RefCell;
use std::rc::Rc;

use chrono::{DateTime, TimeZone, Utc};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::TestBackend;
use ullage_core::summary::{SummaryRow, SummaryValue, UsageSummary};

use super::cards::*;
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
    // Under a day the exact wait is spelled out; within a week each
    // remaining day lights one of seven dots; beyond a week the day count
    // sits centered between diamonds, still seven cells wide.
    let cases = [
        (23 * 86_400, "◆ 23d ◆"),
        (8 * 86_400, "◆  8d ◆"),
        (7 * 86_400, "●●●●●●●"),
        (2 * 86_400, "○○○○○●●"),
        (86_400, "○○○○○○●"),
        (86_400 - 1, "23h59m"),
        (3 * 3_600 + 300, "3h05m"),
        (12 * 60, "12m"),
        (30, "<1m"),
        (-3_600, "<1m"),
    ];
    for (seconds, expected) in cases {
        let rows = rows(&summary(vec![usage_row("5h", 91.0, seconds)]), now());
        let resets = rows[0].resets.as_deref();
        assert_eq!(resets, Some(expected), "{seconds}s");
        if seconds >= 86_400 {
            assert_eq!(
                UnicodeWidthStr::width(expected),
                7,
                "{seconds}s: {expected}"
            );
        }
    }
}

#[test]
fn a_card_costs_two_border_lines_beyond_its_rows_and_notices() {
    let card = Card {
        title: title(),
        rows: vec![row("5h", "remains", "91%", Some("3h05m"))],
        notices: vec!["! stale snapshot".into()],
    };

    assert_eq!(card.height(), 4);
    assert_eq!(card_lines(&card, 42).len(), 4);
}

#[test]
fn a_card_is_a_rounded_box_with_its_title_in_the_top_border() {
    let card = card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]);

    let lines = card_lines(&card, 42);
    let top = line_text(&lines[0]);
    let bottom = line_text(lines.last().unwrap());

    assert!(top.starts_with("╭─ "), "{top}");
    assert!(top.ends_with('╮'), "{top}");
    assert!(top.contains("claude  max_20x  acct-1"), "{top}");
    assert_eq!(UnicodeWidthStr::width(top.as_str()), 42, "{top}");
    assert!(bottom.starts_with('╰'), "{bottom}");
    assert!(bottom.ends_with('╯'), "{bottom}");
    assert_eq!(UnicodeWidthStr::width(bottom.as_str()), 42, "{bottom}");

    // The border is dim, including the cells around the embedded title and
    // the verticals framing each row.
    let dim = |span: &Span<'_>| span.style.add_modifier.contains(Modifier::DIM);
    assert!(dim(&lines[0].spans[0]), "{:?}", lines[0].spans[0]);
    assert!(dim(lines[0].spans.last().unwrap()));
    assert_eq!(lines[1].spans[0].content, "│");
    assert!(dim(&lines[1].spans[0]));
    assert_eq!(lines[1].spans.last().unwrap().content, "│");
    assert!(dim(lines[1].spans.last().unwrap()));
}

#[test]
fn a_full_row_uses_every_column_inside_the_frame() {
    let card = card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]);

    let line = &card_lines(&card, 42)[1];
    let text = line_text(line);

    // The row spans the card between the verticals, and the ten-cell block
    // bar ends just inside the right border.
    assert_eq!(UnicodeWidthStr::width(text.as_str()), 42, "{text}");
    assert!(text.starts_with("│ 5h "), "{text}");
    assert!(text.ends_with("░█████████ │"), "{text}");
    assert!(text.contains("remains 91% 3h05m "), "{text}");
}

#[test]
fn a_42_column_card_still_shows_the_full_ten_cell_bar() {
    let card = card_with_rows(vec![
        row("5h", "remains", "91%", Some("3h05m")),
        CardRow {
            ratio: Some(0.07),
            ..row("weekly", "remains", "7%", Some("○●●●●●●"))
        },
    ]);

    let content = rendered(42, 8, std::slice::from_ref(&card));

    assert!(content.contains("░█████████"), "{content}");
    assert!(content.contains("░░░░░░░░░█"), "{content}");
}

#[test]
fn narrowing_shrinks_the_bar_then_drops_the_verb_the_bar_and_the_countdown() {
    let card = card_with_rows(vec![row("5h", "remains", "91%", Some("3h05m"))]);
    let layout = RowLayout::measure(&card.rows);

    let tiers = [
        (42, Tier::Full, "5h remains 91% 3h05m ░█████████"),
        (29, Tier::MiniBar, "5h remains 91% 3h05m ████"),
        (21, Tier::NoVerb, "5h 91% 3h05m ████"),
        (16, Tier::NoBar, "5h 91% 3h05m"),
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
fn the_title_names_the_provider_first_and_keeps_the_rest_quiet() {
    let spans = title_spans(&title(), 42);
    let text: String = spans.iter().map(|span| span.content.as_ref()).collect();

    assert_eq!(text, "claude  max_20x  acct-1");
    assert!(
        spans[0].style.add_modifier.contains(Modifier::BOLD),
        "{:?}",
        spans[0].style
    );
    // The quiet names are dim, and nothing is padded to the card width.
    let last = spans.last().unwrap();
    assert!(last.style.add_modifier.contains(Modifier::DIM), "{last:?}");
}

#[test]
fn readings_and_countdowns_end_on_the_same_column() {
    let card = card_with_rows(vec![
        row("5h", "remains", "91%", Some("3h05m")),
        row("weekly", "remains", "7%", Some("○●●●●●●")),
    ]);

    let lines = card_lines(&card, 42);
    let first = line_text(&lines[1]);
    let second = line_text(&lines[2]);

    // Right-aligned columns: the percentages and the countdowns line up even
    // though `91%` is one cell wider than `7%`.
    assert_eq!(
        first.find("91%").unwrap() + 3,
        second.find("7%").unwrap() + 2
    );
    assert_eq!(
        first.find("3h05m").unwrap() + 5,
        second.find("○●●●●●●").unwrap() + 7
    );
}

#[test]
fn the_mini_bar_fills_from_the_right_like_the_full_bar() {
    assert_eq!(block_bar(0.0, 4), "░░░░");
    assert_eq!(block_bar(0.25, 4), "░░░█");
    assert_eq!(block_bar(0.5, 4), "░░██");
    assert_eq!(block_bar(1.0, 4), "████");
    assert_eq!(block_bar(2.0, 4), "████", "a ratio above one is clamped");
}

#[test]
fn the_full_bar_is_ten_block_cells_filled_by_the_remaining_ratio() {
    assert_eq!(block_bar(0.0, 10), "░░░░░░░░░░");
    assert_eq!(block_bar(0.25, 10), "░░░░░░░███");
    assert_eq!(block_bar(0.5, 10), "░░░░░█████");
    assert_eq!(block_bar(1.0, 10), "██████████");
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
        for (index, line) in card_lines(&card, width).iter().enumerate() {
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
        Rect::new(0, 6, 42, 6),
        "one blank row between bands"
    );
}

#[test]
fn one_cell_short_of_two_cards_wraps_to_one_column() {
    let rects = card_rects(85, &[3, 3]);
    assert_eq!(rects, [Rect::new(0, 0, 85, 3), Rect::new(0, 4, 85, 3)]);
}

#[test]
fn narrow_screen_compresses_card_to_available_width() {
    assert_eq!(card_rects(12, &[4]), [Rect::new(0, 0, 12, 4)]);
}

#[test]
fn content_height_is_the_lowest_card_bottom() {
    assert_eq!(content_height(&card_rects(86, &[5, 4, 6])), 12);
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
