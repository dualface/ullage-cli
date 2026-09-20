//! Card layout for the full-screen view: what one subscription's box looks
//! like, how its rows keep their columns as the card narrows, and where the
//! boxes land on screen.
//!
//! The box drawing, block bars, and dot countdowns are East Asian ambiguous
//! glyphs: one cell wide to `unicode-width`, possibly two in a terminal set
//! to a CJK locale, where a card can render wider than its allotted rect.

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ullage_core::QueryOutcome;
use ullage_core::summary::UsageSummary;
use ullage_protocol::{SnapshotPayload, SubscriptionUsage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::MetricFilterChoice;
use crate::sanitize_cell;
use crate::table::{
    SECONDS_PER_DAY, Severity, SummaryCells, collect_summary_cells, countdown_text,
    remaining_severity,
};

const PREFERRED_CARD_WIDTH: u16 = 42;
const HORIZONTAL_GAP: u16 = 2;
/// A blank row separates one band of cards from the next.
const VERTICAL_GAP: u16 = 1;
/// One space between the fields of a row. The table can afford two because it
/// owns the whole terminal width; a card has to fit the full bar into a
/// column of about forty cells.
const FIELD_GAP: usize = 1;
/// Two spaces between the names on a title line, which has no columns to keep.
const TITLE_GAP: usize = 2;
/// Cells of the full block bar.
const BAR_WIDTH: usize = 10;
/// Cells of the bar that survives once the full bar no longer fits.
const MINI_BAR_WIDTH: usize = 4;
/// Cells of the reset countdown's dot and diamond fields.
const RESET_FIELD_WIDTH: usize = 7;
/// The dot of a day still to wait, and of a day already counted off.
const DAY_LEFT: char = '•';
const DAY_EMPTY: char = '◦';

/// Renders a card as a rounded box: the title sits in the top border, the
/// rows and the notices are framed by dim verticals, and a bottom border
/// closes the box.
pub(crate) fn card_lines(card: &Card, width: u16) -> Vec<Line<'static>> {
    // One cell of margin inside each vertical, when the card is wide enough
    // to afford it.
    let margin = usize::from(width >= 4);
    let content_width = width.saturating_sub(2 + 2 * margin as u16);
    let measured = RowLayout::measure(&card.rows);
    let tier = measured.tier_for(content_width);
    let layout = measured.stretched(tier, content_width);
    let mut lines = vec![title_border(&card.title, width)];
    lines.extend(
        card.rows
            .iter()
            .map(|row| frame_line(row_line(row, &layout, tier, content_width), width)),
    );
    lines.extend(card.notices.iter().map(|notice| {
        frame_line(
            Line::from(Span::styled(
                clip(notice, usize::from(content_width)),
                Style::default().fg(Color::Yellow),
            )),
            width,
        )
    }));
    lines.push(border_line('╰', '╯', width));
    lines
}

/// `╭─ provider  plan  account ───╮`: the title embedded in the top border,
/// the provider lit and the rest quiet. Below six cells there is no room for
/// a readable title, so the border runs plain.
fn title_border(title: &CardTitle, width: u16) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let width = usize::from(width);
    if width < 6 {
        return border_line('╭', '╮', width as u16);
    }
    // `╭─ ` leads, ` ` plus at least the closing corner follows.
    let spans = title_spans(title, width - 5);
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if used == 0 {
        return border_line('╭', '╮', width as u16);
    }
    let mut line = Vec::with_capacity(spans.len() + 3);
    line.push(Span::styled("╭─ ", dim));
    line.extend(spans);
    line.push(Span::styled(
        format!(" {}", "─".repeat(width - 5 - used)),
        dim,
    ));
    line.push(Span::styled("╮", dim));
    Line::from(line)
}

/// `╰────╯`, or whichever two corners `left` and `right` name, always exactly
/// `width` cells wide.
fn border_line(left: char, right: char, width: u16) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let width = usize::from(width);
    if width == 1 {
        return Line::from(Span::styled(left.to_string(), dim));
    }
    Line::from(vec![
        Span::styled(left.to_string(), dim),
        Span::styled("─".repeat(width - 2), dim),
        Span::styled(right.to_string(), dim),
    ])
}

/// `│ content │`: a card row framed by dim verticals, padded so the right
/// border lands on the card's last cell.
fn frame_line(inner: Line<'static>, width: u16) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let width = usize::from(width);
    if width == 1 {
        return Line::from(Span::styled("│", dim));
    }
    let margin = usize::from(width >= 4);
    let content_width = width - 2 - 2 * margin;
    let used: usize = inner
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let mut spans = Vec::with_capacity(inner.spans.len() + 5);
    spans.push(Span::styled("│", dim));
    if margin > 0 {
        spans.push(Span::raw(" "));
    }
    spans.extend(inner.spans);
    if used < content_width {
        spans.push(Span::raw(" ".repeat(content_width - used)));
    }
    if margin > 0 {
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("│", dim));
    Line::from(spans)
}

/// The title as styled spans clipped to `budget` cells: the provider lit,
/// the plan and the account quiet beside it. Whitespace separates them, so
/// no punctuation has to be picked that a terminal might render two cells
/// wide.
pub(crate) fn title_spans(title: &CardTitle, budget: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0;
    let mut push = |text: &str, style: Style, spans: &mut Vec<Span<'static>>| {
        if text.is_empty() || used >= budget {
            return;
        }
        if !spans.is_empty() {
            let gap = TITLE_GAP.min(budget - used);
            spans.push(Span::raw(" ".repeat(gap)));
            used += gap;
        }
        let text = clip(text, budget - used);
        used += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, style));
    };
    let quiet = Style::default().add_modifier(Modifier::DIM);
    push(
        &title.provider,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        &mut spans,
    );
    push(title.plan.as_deref().unwrap_or(""), quiet, &mut spans);
    push(&title.account, quiet, &mut spans);
    spans
}

/// How much room each field of a card's rows needs, before any is dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowLayout {
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
    /// The four-cell bar that replaces the full one on a narrow terminal.
    mini: usize,
}

/// The fields a row keeps at a given width, widest variant first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Identity, reading, reset countdown, full progress bar.
    Full,
    /// The full bar shrinks to four cells before anything else is dropped.
    MiniBar,
    /// The verb is prose; the number it introduces carries the meaning.
    NoVerb,
    /// Even the mini bar goes before the countdown does.
    NoBar,
    /// The countdown goes last, after everything but identity and reading.
    NoReset,
    /// Identity and reading only, both clipped to fit.
    Minimal,
}

impl RowLayout {
    pub(crate) fn measure(rows: &[CardRow]) -> Self {
        Self {
            identity: max_width(rows.iter().map(|row| row.identity.as_str())),
            verb: max_width(rows.iter().map(|row| row.verb)),
            amount: max_width(rows.iter().map(|row| row.amount.as_str())),
            reading: max_width(rows.iter().map(reading)),
            suffix: max_width(rows.iter().map(|row| row.suffix)),
            resets: max_width(rows.iter().filter_map(|row| row.resets.as_deref())),
            bar: if rows.iter().any(|row| row.ratio.is_some()) {
                BAR_WIDTH
            } else {
                0
            },
            mini: if rows.iter().any(|row| row.ratio.is_some()) {
                MINI_BAR_WIDTH
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
    pub(crate) fn tier_for(&self, width: u16) -> Tier {
        let width = usize::from(width);
        for tier in [
            Tier::Full,
            Tier::MiniBar,
            Tier::NoVerb,
            Tier::NoBar,
            Tier::NoReset,
        ] {
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
            Tier::MiniBar => &[
                self.identity,
                self.verb,
                self.amount,
                self.suffix,
                self.resets,
                self.mini,
            ],
            Tier::NoVerb => &[
                self.identity,
                self.reading,
                self.suffix,
                self.resets,
                self.mini,
            ],
            Tier::NoBar => &[self.identity, self.reading, self.suffix, self.resets],
            Tier::NoReset => &[self.identity, self.reading, self.suffix],
            Tier::Minimal => &[self.identity, self.reading],
        };
        let used: usize = fields.iter().filter(|field| **field > 0).sum();
        let gaps = fields.iter().filter(|field| **field > 0).count();
        used + FIELD_GAP * gaps.saturating_sub(1)
    }
}

pub(crate) fn row_line(row: &CardRow, layout: &RowLayout, tier: Tier, width: u16) -> Line<'static> {
    if tier == Tier::Minimal {
        return minimal_line(row, width);
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    push_field(&mut spans, &row.identity, layout.identity, Style::default());
    // Numbers read as a column only when they end on the same cell.
    if matches!(tier, Tier::Full | Tier::MiniBar) {
        push_field(&mut spans, row.verb, layout.verb, dim);
        push_aligned(
            &mut spans,
            &row.amount,
            layout.amount,
            amount_style(row),
            Align::Right,
        );
    } else {
        push_aligned(
            &mut spans,
            reading(row),
            layout.reading,
            amount_style(row),
            Align::Right,
        );
    }
    push_field(&mut spans, row.suffix, layout.suffix, dim);
    if tier != Tier::NoReset {
        push_aligned(
            &mut spans,
            row.resets.as_deref().unwrap_or(""),
            layout.resets,
            dim,
            Align::Right,
        );
    }
    match tier {
        Tier::Full => push_field(
            &mut spans,
            &row.ratio
                .map(|ratio| block_bar(ratio, BAR_WIDTH))
                .unwrap_or_default(),
            layout.bar,
            amount_style(row),
        ),
        Tier::MiniBar | Tier::NoVerb => push_field(
            &mut spans,
            &row.ratio
                .map(|ratio| block_bar(ratio, MINI_BAR_WIDTH))
                .unwrap_or_default(),
            layout.mini,
            amount_style(row),
        ),
        _ => {}
    }
    Line::from(spans)
}

/// The cell of a bar that is still quota, and the cell that is spent.
const BAR_FILLED: char = '━';
const BAR_EMPTY: char = '┄';

/// The remaining ratio as a heavy-and-dashed rule, filled from the right
/// exactly as the table's bar fills its brackets.
pub(crate) fn block_bar(remaining_ratio: f64, cells: usize) -> String {
    let filled = (remaining_ratio.clamp(0.0, 1.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!(
        "{}{}",
        BAR_EMPTY.to_string().repeat(cells - filled),
        BAR_FILLED.to_string().repeat(filled)
    )
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
    push_aligned(spans, text, width, style, Align::Left);
}

/// Which edge of its column a field sits against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

fn push_aligned(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    width: usize,
    style: Style,
    align: Align,
) {
    if width == 0 {
        return;
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" ".repeat(FIELD_GAP)));
    }
    let text = clip(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    if padding > 0 && align == Align::Right {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(text, style));
    if padding > 0 && align == Align::Left {
        spans.push(Span::raw(" ".repeat(padding)));
    }
}

/// Cuts `text` to `width` display cells, never splitting a wide character.
pub(crate) fn clip(text: &str, width: usize) -> String {
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
pub(crate) struct Card {
    pub(crate) title: CardTitle,
    pub(crate) rows: Vec<CardRow>,
    pub(crate) notices: Vec<String>,
}

impl Card {
    /// The rows and the notices, plus the top and bottom borders.
    pub(crate) fn height(&self) -> u16 {
        (self.rows.len() + self.notices.len() + 2)
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

/// The three names that identify a subscription, kept apart so each can be
/// styled on its own.
#[derive(Debug)]
pub(crate) struct CardTitle {
    pub(crate) provider: String,
    pub(crate) plan: Option<String>,
    pub(crate) account: String,
}

#[derive(Debug)]
pub(crate) struct CardRow {
    pub(crate) identity: String,
    pub(crate) verb: &'static str,
    pub(crate) amount: String,
    pub(crate) suffix: &'static str,
    pub(crate) resets: Option<String>,
    pub(crate) ratio: Option<f64>,
}

pub(crate) fn cards(snapshots: &[SnapshotPayload], now: DateTime<Utc>) -> Vec<Card> {
    let mut cards = snapshots
        .iter()
        .map(|snapshot| card(snapshot, now))
        .collect::<Vec<_>>();
    // The daemon hands the snapshots over in its own order, which changes as
    // accounts are added; the view reads better when a subscription sits
    // where it sat last time.
    cards.sort_by_key(|card| sort_key(&card.title));
    cards
}

/// Provider first, then account, neither case deciding the order. The
/// original spelling breaks a tie so two accounts differing only in case
/// keep a stable order.
fn sort_key(title: &CardTitle) -> (String, String, String, String) {
    (
        title.provider.to_lowercase(),
        title.account.to_lowercase(),
        title.provider.clone(),
        title.account.clone(),
    )
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

fn card_title(usage: &SubscriptionUsage, account_id: &str) -> CardTitle {
    CardTitle {
        provider: sanitize_cell(usage.provider.as_str()).to_owned(),
        plan: usage
            .plan
            .as_deref()
            .filter(|plan| !plan.trim().is_empty())
            .map(|plan| sanitize_cell(plan).to_owned()),
        account: sanitize_cell(account_id).to_owned(),
    }
}

/// Reuses the table's cells, so the view names windows, metrics, and readings
/// exactly as `show` does, and hides the same rows.
pub(crate) fn rows(summary: &UsageSummary, now: DateTime<Utc>) -> Vec<CardRow> {
    let mut rows = summary_rows(summary, now);
    strip_shared_prefix(&mut rows);
    rows
}

fn summary_rows(summary: &UsageSummary, now: DateTime<Utc>) -> Vec<CardRow> {
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
                resets: resets_in.map(reset_text),
                ratio: remaining_ratio,
            }
        })
        .collect()
}

/// The characters that join a window to a metric in a row's name.
const NAME_SEPARATORS: [char; 3] = ['-', '_', ' '];

/// Drops a leading name every row of the card repeats, with the separator
/// behind it: Cursor calls all of its windows `monthly-...`, and the word
/// says nothing once it is on every row. A card keeps the name when a row
/// would be left with nothing of its own, and a single-row card keeps it
/// too, because there is no repetition to remove.
pub(crate) fn strip_shared_prefix(rows: &mut [CardRow]) {
    while let Some(prefix) = shared_prefix(rows) {
        for row in rows.iter_mut() {
            row.identity = row.identity[prefix..]
                .trim_start_matches(NAME_SEPARATORS)
                .to_owned();
        }
    }
}

/// The length of the leading segment every row shares, when dropping it
/// leaves each row a name.
fn shared_prefix(rows: &[CardRow]) -> Option<usize> {
    if rows.len() < 2 {
        return None;
    }
    let first = rows.first()?.identity.as_str();
    let prefix = first.find(NAME_SEPARATORS)?;
    if prefix == 0 {
        return None;
    }
    rows.iter()
        .all(|row| {
            row.identity.get(..prefix) == Some(&first[..prefix])
                && !row.identity[prefix..]
                    .trim_start_matches(NAME_SEPARATORS)
                    .is_empty()
                && row.identity[prefix..].starts_with(NAME_SEPARATORS)
        })
        .then_some(prefix)
}

/// How long until the window resets. Under a day the exact wait is worth
/// more than the scale, so it is spelled out (`3h05m`, `12m`, `<1m`); within
/// a week each remaining day lights one of seven dots (`◦◦◦◦◦••`); beyond a
/// week the day count sits centered between two diamonds (`◆ 23d ◆`).
fn reset_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / SECONDS_PER_DAY;
    if days > RESET_FIELD_WIDTH as i64 {
        return diamond_field(&format!("{days}d"));
    }
    if days >= 1 {
        let days = days as usize;
        return format!(
            "{}{}",
            DAY_EMPTY.to_string().repeat(RESET_FIELD_WIDTH - days),
            DAY_LEFT.to_string().repeat(days)
        );
    }
    countdown_text(seconds)
}

/// `◆ 23d ◆`: the day count centered in a [`RESET_FIELD_WIDTH`]-cell field
/// between diamonds, the left side taking the extra cell when the padding is
/// odd.
fn diamond_field(text: &str) -> String {
    // The clip keeps absurdly far-out resets inside the field: two diamonds
    // plus a clipped count still read as a date far away.
    let text = clip(text, RESET_FIELD_WIDTH - 2);
    let pad = RESET_FIELD_WIDTH.saturating_sub(text.chars().count() + 2);
    let left = pad.div_ceil(2);
    format!("◆{}{}{}◆", " ".repeat(left), text, " ".repeat(pad - left))
}

fn usage_data(outcome: &QueryOutcome<SubscriptionUsage>) -> &SubscriptionUsage {
    match outcome {
        QueryOutcome::Complete { data } | QueryOutcome::Partial { data, .. } => data,
    }
}

pub(crate) fn card_rects(width: u16, heights: &[u16], vertical: bool) -> Vec<Rect> {
    if width == 0 || heights.is_empty() {
        return Vec::new();
    }
    let columns = ((u32::from(width) + u32::from(HORIZONTAL_GAP))
        / u32::from(PREFERRED_CARD_WIDTH + HORIZONTAL_GAP))
    .max(1) as u16;
    // A band holding one card keeps the card's own width and centers it:
    // stretched across the terminal, the row's reading would be stranded at
    // the far edge, and pinned left it would sit under a lopsided margin.
    if vertical || columns == 1 {
        return stacked_rects(width, heights);
    }
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

/// One card per band, each at the card's own width and centered.
fn stacked_rects(width: u16, heights: &[u16]) -> Vec<Rect> {
    let card_width = width.min(PREFERRED_CARD_WIDTH);
    let x = (width - card_width) / 2;
    let mut y = 0u16;
    heights
        .iter()
        .map(|height| {
            let rect = Rect::new(x, y, card_width, *height);
            y = y.saturating_add(height + VERTICAL_GAP);
            rect
        })
        .collect()
}

pub(crate) fn content_height(rects: &[Rect]) -> u16 {
    rects.iter().map(|rect| rect.bottom()).max().unwrap_or(0)
}
