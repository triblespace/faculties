//! Recorded quota in the same frozen workspace view as the other Data cards.

use std::collections::{BTreeMap, BTreeSet};

use egui::{Align2, Pos2, Rect, Sense, Stroke, TextStyle, Ui};
use GORBIE::dataflow::ComputedState;
use GORBIE::prelude::CardCtx;

use crate::quota::history::rows_in_range;
use triblespace::core::{metadata, repo::BlobStoreGet};
use triblespace::prelude::*;

use crate::quota::render::{format_age, format_percent, format_window, remaining_percent};
use crate::quota::{reading_values, tai_ns_to_unix, unix_to_tai_ns, GroupKey, STALE_AFTER_SECONDS};
use crate::widgets::{DatasetRevision, DatasetView, SourceKey, WidgetContext};

const SECOND: i128 = 1_000_000_000;
const DAY: i64 = 86_400;
const MAX_ROWS: usize = 8_192;
const MAX_VALUES: usize = 16_384;
const MAX_VALUES_PER_READING: usize = 64;
const MAX_ALLOWANCES: usize = 256;
const PAGE_SIZE: usize = 8;
const MAX_LABEL_BYTES: usize = 128;
const RANGES: [(i64, &str); 4] = [
    (3_600, "1h"),
    (DAY, "24h"),
    (7 * DAY, "7d"),
    (30 * DAY, "30d"),
];
const SECTION: &str = "Quota";

pub struct QuotaViewer {
    range: usize,
    allowance_page: usize,
    selected: Option<GroupKey>,
    inspected_at: Option<i128>,
    reading_page: usize,
    result: ComputedState<Option<(ViewKey, Projection)>>,
}

impl Default for QuotaViewer {
    fn default() -> Self {
        Self {
            range: 2,
            allowance_page: 0,
            selected: None,
            inspected_at: None,
            reading_page: 0,
            result: ComputedState::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewKey {
    dataset: DatasetRevision,
    range: usize,
    minute: i64,
}

enum Projection {
    Ready(History),
    TooManyReadings,
    Unavailable,
}

struct History {
    since: i128,
    until: i128,
    allowances: Vec<Allowance>,
}

struct Allowance {
    key: GroupKey,
    label: String,
    readings: Vec<Reading>,
}

struct Reading {
    at: i128,
    used: Vec<f64>,
    resets: Vec<i128>,
}

impl Reading {
    fn age(&self, now: i128) -> i64 {
        i64::try_from(now.saturating_sub(self.at) / SECOND).unwrap_or(i64::MAX)
    }

    fn stale(&self, now: i128) -> bool {
        self.age(now) > STALE_AFTER_SECONDS
    }

    fn amount(&self) -> String {
        match self.used.as_slice() {
            [] => "Usage unavailable".into(),
            [used] if valid_usage(*used) => {
                format!("{} remaining", format_percent(remaining_percent(*used)))
            }
            [..] => "See reported usage values".into(),
        }
    }

    fn status(&self, now: i128) -> String {
        let age = self.age(now);
        let reset = match self.resets.as_slice() {
            [reset] if now >= *reset => "reset since reading".into(),
            [reset] => format!(
                "resets in {}",
                format_age(i64::try_from(reset.saturating_sub(now) / SECOND).unwrap_or(i64::MAX))
            ),
            [] => "reset time unavailable".into(),
            _ => "multiple reported reset times".into(),
        };
        let stale = if self.stale(now) { " · stale" } else { "" };
        format!("{} ago{stale} · {reset}", format_age(age))
    }
}

fn valid_usage(used: f64) -> bool {
    used.is_finite() && used >= 0.0
}

// Only this bounded presentation is retained. Keys, values and times are
// resolved from the selected observation when its revision or window changes.
fn project(view: DatasetView<'_>, now: i128, seconds: i64) -> Projection {
    let until = tai_ns_to_unix(now).saturating_add(1);
    let since = until.saturating_sub(seconds);
    let Ok(rows) = rows_in_range(view.facts, since, until) else {
        return Projection::Unavailable;
    };
    // These maps only order the bounded results of this query.
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    let mut remaining_values = MAX_VALUES;
    for (index, row) in rows.enumerate() {
        if index >= MAX_ROWS {
            return Projection::TooManyReadings;
        }
        if row.observed > now {
            continue;
        }
        let (used, resets) = reading_values(view.facts, row.event);
        let limit = remaining_values.min(MAX_VALUES_PER_READING);
        let used = used.take(limit + 1).collect::<Vec<_>>();
        if used.len() > limit {
            return Projection::TooManyReadings;
        }
        let limit = limit - used.len();
        let mut resets = resets.take(limit + 1).collect::<Vec<_>>();
        if resets.len() > limit {
            return Projection::TooManyReadings;
        }
        remaining_values -= used.len() + resets.len();
        resets.sort_unstable();
        groups.entry(row.key).or_default().push((
            row.event,
            Reading {
                at: row.observed,
                used,
                resets,
            },
        ));
        if groups.len() > MAX_ALLOWANCES {
            return Projection::TooManyReadings;
        }
    }
    let mut allowances = groups
        .into_iter()
        .map(|(key, mut rows)| {
            // Same total order as Quota's latest reading.
            rows.sort_by_key(|(event, reading)| (reading.at, *event));
            Allowance {
                key,
                label: allowance_label(view, key),
                readings: rows.into_iter().map(|(_, reading)| reading).collect(),
            }
        })
        .collect::<Vec<_>>();
    allowances.sort_by(|a, b| a.label.cmp(&b.label));
    Projection::Ready(History {
        since: unix_to_tai_ns(since),
        until: now,
        allowances,
    })
}

fn allowance_label(view: DatasetView<'_>, key: GroupKey) -> String {
    let provider = key.provider;
    let name = crate::quota::known_provider_name(key.provider)
        .map(str::to_owned)
        .or_else(|| {
            find!(
                name: crate::quota::TextHandle,
                pattern!(view.facts, [{ provider @ metadata::name: ?name }])
            )
            .next()
            .and_then(|handle| display_text(view.reader, handle))
        });
    let provider = match name.as_deref() {
        Some("claude") => "Claude".into(),
        Some("codex") => "Codex".into(),
        Some(name) => name.to_owned(),
        None => format!("Provider {}", &format!("{:x}", key.provider)[..8]),
    };
    // A display alias derived from the handle, never the account text or
    // a prefix of what the writer supplied. Stable across ranges and windows.
    let account = blake3::derive_key("Faculties Quota display account v1", &key.account.raw);
    let account = hex::encode_upper(&account[..5]);
    let window = if key.window % SECOND == 0 {
        i64::try_from(key.window / SECOND)
            .map(format_window)
            .unwrap_or_else(|_| format!("{}s", key.window / SECOND))
    } else {
        format!("{}ns", key.window)
    };
    match key.bucket {
        Some(handle) => {
            let bucket = display_text(view.reader, handle).unwrap_or_else(|| {
                format!("bucket {} label unavailable", hex::encode(&handle.raw[..5]))
            });
            format!("{provider} · Account {account} · {window} · {bucket}")
        }
        None => format!("{provider} · Account {account} · {window} · No bucket"),
    }
}

// Read from the already-selected snapshot, not an acquiring reader. Inspect
// byte length before UTF-8 validation or String allocation. Oversized labels
// remain distinguishable without decoding/copying their text; GroupKey
// continues to retain the full native handle, never this display alias.
// The store still validates content identity on get: this is a bound on
// display decoding/allocation/layout, not a new bound on storage validation.
fn display_text(reader: &impl BlobStoreGet, handle: crate::quota::TextHandle) -> Option<String> {
    let blob: Blob<blobencodings::UTF8String> = reader.get(handle).ok()?;
    if blob.bytes.len() > MAX_LABEL_BYTES {
        return Some(format!("long label [{}]", hex::encode(&handle.raw[..5])));
    }
    std::str::from_utf8(&blob.bytes).ok().map(str::to_owned)
}

impl QuotaViewer {
    pub fn render(&mut self, ctx: &mut CardCtx<'_>, sources: WidgetContext<'_>) {
        ctx.section(SECTION, |ctx| {
            ctx.with_padding(12, |ctx| self.show(ctx.ui_mut(), sources));
        });
    }

    fn show(&mut self, ui: &mut Ui, sources: WidgetContext<'_>) {
        let Some(dataset) = sources.owned_dataset(SourceKey::Quota) else {
            ui.label(if sources.missing_route(SourceKey::Quota) {
                "Quota route missing from this workspace."
            } else {
                "Quota collection unavailable."
            });
            return;
        };
        let Ok(now) = crate::clock::tai_nanoseconds_now() else {
            ui.label("Current time unavailable.");
            return;
        };
        // A read that could not load every foundation still shows what is
        // here, and says what it omits.
        let notes = sources.notes(SourceKey::Quota);
        for note in notes {
            ui.small(format!("Showing what is here: {note}"));
        }
        ui.horizontal(|ui| {
            ui.label("History");
            for (index, (_, label)) in RANGES.iter().enumerate() {
                if ui
                    .selectable_value(&mut self.range, index, *label)
                    .changed()
                {
                    self.allowance_page = 0;
                    self.inspected_at = None;
                    self.reading_page = 0;
                }
            }
        });

        let key = ViewKey {
            dataset: sources
                .revision(SourceKey::Quota)
                .expect("owned dataset has a revision"),
            range: self.range,
            minute: tai_ns_to_unix(now).div_euclid(60),
        };
        let result = GORBIE::widgets::load_auto(
            ui,
            &mut self.result,
            |value| value.as_ref().is_none_or(|(cached, _)| *cached != key),
            move || {
                let result = match dataset.view() {
                    Ok(view) => project(view, now, RANGES[key.range].0),
                    Err(_) => Projection::Unavailable,
                };
                Some((key, result))
            },
        );
        match result.as_ref().filter(|(cached, _)| *cached == key) {
            Some((_, Projection::Ready(history))) if history.allowances.is_empty() => {
                ui.label(empty_range_message(notes));
            }
            Some((_, Projection::Ready(history))) => draw_history(
                ui,
                history,
                now,
                &mut self.selected,
                &mut self.allowance_page,
                &mut self.inspected_at,
                &mut self.reading_page,
            ),
            Some((_, Projection::TooManyReadings)) => {
                ui.label("Quota history exceeds the display limit. Try a shorter range.");
            }
            Some((_, Projection::Unavailable)) => {
                ui.label("Quota history unavailable.");
            }
            None => {
                ui.label("Loading quota history…");
            }
        }
    }
}

/// An empty range over a complete read is empty; over a partial one it is
/// only empty here.
fn empty_range_message(notes: &[String]) -> &'static str {
    if notes.is_empty() {
        "No quota observations in this range."
    } else {
        "No quota observations here in this range; some could not be loaded."
    }
}

fn draw_history(
    ui: &mut Ui,
    history: &History,
    now: i128,
    selected: &mut Option<GroupKey>,
    allowance_page: &mut usize,
    inspected_at: &mut Option<i128>,
    reading_page: &mut usize,
) {
    let allowances = &history.allowances;
    if !allowances
        .iter()
        .any(|allowance| Some(allowance.key) == *selected)
    {
        *selected = Some(allowances[0].key);
        *inspected_at = None;
        *reading_page = 0;
    }
    ui.small("Latest readings in the selected range");
    for allowance in paged(ui, allowances, allowance_page) {
        let latest = allowance
            .readings
            .last()
            .expect("one or more observations in an allowance");
        let title = format!("{}   {}", allowance.label, latest.amount());
        if ui
            .selectable_label(*selected == Some(allowance.key), title)
            .clicked()
        {
            *selected = Some(allowance.key);
            *inspected_at = None;
            *reading_page = 0;
        }
        ui.weak(latest.status(now));
    }
    let allowance = allowances
        .iter()
        .find(|entry| Some(entry.key) == *selected)
        .expect("selection resolves to an allowance");
    ui.separator();
    ui.strong(&allowance.label);
    ui.small("Observed remaining allowance · UTC · Blank periods have no readings");
    let latest = allowance
        .readings
        .last()
        .expect("one or more observations in an allowance")
        .at;
    let at = inspected_at.unwrap_or(latest);
    let at = if allowance.readings.iter().any(|reading| reading.at == at) {
        at
    } else {
        latest
    };
    let clicked = draw_chart(ui, history, &allowance.readings, at);
    let earlier = allowance
        .readings
        .iter()
        .rev()
        .find(|reading| reading.at < at);
    let later = allowance.readings.iter().find(|reading| reading.at > at);
    let mut chosen = clicked.unwrap_or(at);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(earlier.is_some(), egui::Button::new("Earlier reading"))
            .clicked()
        {
            chosen = earlier.expect("enabled").at;
        }
        ui.label(utc(chosen, "%d %b %Y %H:%M:%S"));
        if ui
            .add_enabled(later.is_some(), egui::Button::new("Later reading"))
            .clicked()
        {
            chosen = later.expect("enabled").at;
        }
    });
    if chosen != at {
        *inspected_at = Some(chosen);
        *reading_page = 0;
    }
    let first = allowance
        .readings
        .partition_point(|reading| reading.at < chosen);
    let end = allowance
        .readings
        .partition_point(|reading| reading.at <= chosen);
    for reading in paged(ui, &allowance.readings[first..end], reading_page) {
        let used = if reading.used.is_empty() {
            "Usage unavailable".into()
        } else {
            reading
                .used
                .iter()
                .map(|used| {
                    if valid_usage(*used) && *used > 100.0 {
                        format!(
                            "{used}% used · 0% remaining · {} over",
                            format_percent(*used - 100.0)
                        )
                    } else {
                        format!("{used}% used")
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };
        let resets = if reading.resets.is_empty() {
            "Reset time unavailable".into()
        } else {
            let times = reading
                .resets
                .iter()
                .map(|at| utc(*at, "%d %b %H:%M:%S"))
                .collect::<Vec<_>>()
                .join(" / ");
            format!("Reported reset {times}")
        };
        ui.small(format!("{used}  ·  {resets}"));
    }
}

fn paged<'a, T>(ui: &mut Ui, rows: &'a [T], page: &mut usize) -> &'a [T] {
    let pages = rows.len().div_ceil(PAGE_SIZE).max(1);
    *page = (*page).min(pages - 1);
    if pages > 1 {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(*page > 0, egui::Button::new("Previous page"))
                .clicked()
            {
                *page -= 1;
            }
            ui.small(format!("Page {} of {pages}", *page + 1));
            if ui
                .add_enabled(*page + 1 < pages, egui::Button::new("Next page"))
                .clicked()
            {
                *page += 1;
            }
        });
    }
    let start = *page * PAGE_SIZE;
    &rows[start..(start + PAGE_SIZE).min(rows.len())]
}

fn utc(at: i128, format: &str) -> String {
    chrono::DateTime::from_timestamp(tai_ns_to_unix(at), 0)
        .map(|time| time.format(format).to_string())
        .unwrap_or_else(|| "Time unavailable".into())
}

fn draw_chart(
    ui: &mut Ui,
    history: &History,
    readings: &[Reading],
    selected: i128,
) -> Option<i128> {
    let (response, painter) =
        ui.allocate_painter(egui::vec2(ui.available_width(), 170.0), Sense::click());
    let plot = Rect::from_min_max(
        response.rect.min + egui::vec2(40.0, 8.0),
        response.rect.max - egui::vec2(8.0, 22.0),
    );
    if plot.width() <= 0.0 {
        return None;
    }
    let font = TextStyle::Small.resolve(ui.style());
    let ink = ui.visuals().text_color();
    let grid = Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let accent = GORBIE::themes::button_light_on();
    let duration = (history.until - history.since).max(1) as f64;
    let x = |at| plot.left() + plot.width() * ((at - history.since) as f64 / duration) as f32;
    let y = |remaining| plot.bottom() - plot.height() * (remaining / 100.0) as f32;
    for remaining in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let y = y(remaining);
        painter.hline(plot.x_range(), y, grid);
        painter.text(
            Pos2::new(plot.left() - 5.0, y),
            Align2::RIGHT_CENTER,
            format!("{remaining}%"),
            font.clone(),
            ink,
        );
    }
    for (at, align) in [
        (history.since, Align2::LEFT_TOP),
        (history.until, Align2::RIGHT_TOP),
    ] {
        painter.text(
            Pos2::new(x(at), plot.bottom() + 6.0),
            align,
            utc(at, "%d %b %H:%M"),
            font.clone(),
            ink,
        );
    }
    let resets = readings
        .iter()
        .flat_map(|reading| {
            reading
                .resets
                .iter()
                .copied()
                .filter(|at| *at >= reading.at)
        })
        .filter(|at| (history.since..=history.until).contains(at))
        .collect::<BTreeSet<_>>();
    for reset in &resets {
        painter.vline(
            x(*reset),
            plot.top()..=plot.top() + 8.0,
            Stroke::new(2.0, ink),
        );
    }
    if !resets.is_empty() {
        ui.small("Ticks above the plot mark reported reset times.");
    }
    let mut plotted = false;
    for reading in readings {
        for used in reading
            .used
            .iter()
            .copied()
            .filter(|used| valid_usage(*used))
        {
            let point = Pos2::new(x(reading.at), y(remaining_percent(used)));
            painter.circle_filled(point, 3.0, accent);
            if reading.at == selected {
                painter.circle_stroke(point, 5.0, Stroke::new(1.0, ink));
            }
            plotted = true;
        }
    }
    if !plotted {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "No plottable usage values",
            font,
            ink,
        );
    }
    let hovered = response.hover_pos()?;
    let nearest = readings.iter().min_by(|a, b| {
        (x(a.at) - hovered.x)
            .abs()
            .total_cmp(&(x(b.at) - hovered.x).abs())
    })?;
    let close = plot.expand(5.0).contains(hovered) && (x(nearest.at) - hovered.x).abs() <= 8.0;
    let chosen = (close && response.clicked()).then_some(nearest.at);
    if close {
        response.on_hover_text(format!(
            "{} UTC · Click to inspect",
            utc(nearest.at, "%d %b %H:%M:%S")
        ));
    }
    chosen
}

#[cfg(test)]
mod tests;
