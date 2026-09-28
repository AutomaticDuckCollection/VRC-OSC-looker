//! Terminal UI: view state, key handling and drawing.
//!
//! The UI only reads the store (or a frozen copy of it while paused). All
//! text it draws from the wire was sanitized when it entered the store.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row as TableRow, Sparkline, Table, TableState, Tabs};

use crate::export;
use crate::net::Counters;
use crate::state::sanitize;
use crate::state::{EventKind, Row, Store, Value};

/// Rows not updated for this long are dimmed.
const STALE_AFTER: Duration = Duration::from_secs(5);
/// Values that changed this recently are highlighted.
const CHANGE_HIGHLIGHT: Duration = Duration::from_millis(1200);
/// How long a status message (export path, errors) stays up.
const MESSAGE_TIME: Duration = Duration::from_secs(10);
const MAX_FILTER_CHARS: usize = 64;
/// Below this width the float min/max/peak columns are dropped from the table.
const WIDE_LAYOUT: u16 = 110;

const DIM: Style = Style::new().fg(Color::DarkGray);
const HEADER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CHANGED: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Live,
    Blips,
    Events,
}

impl Pane {
    const ALL: [Pane; 3] = [Pane::Live, Pane::Blips, Pane::Events];

    fn idx(self) -> usize {
        self as usize
    }

    fn cycle(self, forward: bool) -> Pane {
        let i = self.idx() + if forward { 1 } else { 2 };
        Pane::ALL[i % 3]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Name,
    LastUpdate,
    Count,
}

impl Sort {
    fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::LastUpdate,
            Sort::LastUpdate => Sort::Count,
            Sort::Count => Sort::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::LastUpdate => "last update",
            Sort::Count => "count",
        }
    }
}

/// What the selection is pinned to, so it survives re-sorting and new items.
#[derive(Clone, Debug, PartialEq)]
enum SelKey {
    Addr(Rc<str>),
    Seq(u64),
}

#[derive(Default)]
struct Selection {
    index: usize,
    key: Option<SelKey>,
    offset: usize,
}

struct Message {
    text: String,
    error: bool,
    at: Instant,
}

/// Pure view state (no data).
struct View {
    pane: Pane,
    sort: Sort,
    noise_filter: bool,
    filter: String,
    filter_lc: String,
    editing_filter: bool,
    confirm_clear: bool,
    history: Option<Rc<str>>,
    sel: [Selection; 3],
    page: usize,
    message: Option<Message>,
}

pub struct App {
    store: Store,
    /// Frozen copy of the store and the instant it was taken, while paused.
    paused: Option<(Store, Instant)>,
    view: View,
    listen: String,
    counters: Arc<Counters>,
    export_raw: bool,
    pub quit: bool,
}

impl App {
    pub fn new(store: Store, listen: String, counters: Arc<Counters>, export_raw: bool) -> Self {
        Self {
            store,
            paused: None,
            view: View {
                pane: Pane::Live,
                sort: Sort::Name,
                noise_filter: true,
                filter: String::new(),
                filter_lc: String::new(),
                editing_filter: false,
                confirm_clear: false,
                history: None,
                sel: Default::default(),
                page: 10,
                message: None,
            },
            listen,
            counters,
            export_raw,
            quit: false,
        }
    }

    pub fn ingest(&mut self, bytes: &[u8], at: Instant) {
        self.store.ingest_packet(bytes, at);
    }

    #[cfg(test)]
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) {
        let v = &mut self.view;
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C')) {
            self.quit = true;
            return;
        }

        if v.confirm_clear {
            v.confirm_clear = false;
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                self.store.clear();
                if let Some((snap, _)) = &mut self.paused {
                    snap.clear();
                }
                v.history = None;
                v.sel = Default::default();
                v.flash("Cleared all recorded data.", false, now);
            } else {
                v.flash("Clear cancelled.", false, now);
            }
            return;
        }

        if v.editing_filter {
            match key.code {
                KeyCode::Enter => v.editing_filter = false,
                KeyCode::Esc => {
                    v.filter.clear();
                    v.editing_filter = false;
                }
                KeyCode::Backspace => {
                    v.filter.pop();
                }
                KeyCode::Char(c) if !c.is_control() && v.filter.chars().count() < MAX_FILTER_CHARS => v.filter.push(c),
                _ => {}
            }
            v.filter_lc = v.filter.to_lowercase();
            return;
        }

        match key.code {
            KeyCode::Char('q' | 'Q') => self.quit = true,
            KeyCode::Char('p' | 'P') => {
                self.paused = match self.paused.take() {
                    Some(_) => None,
                    None => Some((self.store.clone(), now)),
                };
            }
            KeyCode::Char('/') => v.editing_filter = true,
            KeyCode::Char('s' | 'S') => v.sort = v.sort.next(),
            KeyCode::Tab => v.pane = v.pane.cycle(true),
            KeyCode::BackTab => v.pane = v.pane.cycle(false),
            KeyCode::Char('n' | 'N') => v.noise_filter = !v.noise_filter,
            KeyCode::Char('c' | 'C') => v.confirm_clear = true,
            KeyCode::Char('e' | 'E') => self.export(now),
            KeyCode::Esc => {
                if v.history.is_some() {
                    v.history = None;
                } else {
                    v.filter.clear();
                    v.filter_lc.clear();
                }
            }
            KeyCode::Enter => {
                if v.history.is_some() {
                    v.history = None;
                } else {
                    let store = self.paused.as_ref().map_or(&self.store, |(s, _)| s);
                    let list = v.visible(store, v.pane);
                    let i = v.resolve(store, v.pane, &list);
                    v.history = list.get(i).and_then(|&item| v.addr_of(store, v.pane, item));
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                let store = self.paused.as_ref().map_or(&self.store, |(s, _)| s);
                let list = v.visible(store, v.pane);
                if list.is_empty() {
                    return;
                }
                let cur = v.resolve(store, v.pane, &list) as isize;
                let page = v.page.max(1) as isize;
                let last = list.len() as isize - 1;
                let next = match key.code {
                    KeyCode::Up => cur - 1,
                    KeyCode::Down => cur + 1,
                    KeyCode::PageUp => cur - page,
                    KeyCode::PageDown => cur + page,
                    KeyCode::Home => 0,
                    _ => last,
                }
                .clamp(0, last) as usize;
                v.select(store, v.pane, &list, next);
                if v.history.is_some() {
                    v.history = v.addr_of(store, v.pane, list[next]);
                }
            }
            _ => {}
        }
    }

    fn export(&mut self, now: Instant) {
        let (store, at) = match &self.paused {
            Some((s, t)) => (s, *t),
            None => (&self.store, now),
        };
        let net =
            export::NetCounts { queue_full: self.counters.queue_full(), non_loopback: self.counters.non_loopback() };
        let doc = export::snapshot(store, at, self.export_raw, &net);
        match export::write_new(Path::new("."), &doc) {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let full = std::path::absolute(&path).unwrap_or(path);
                let note = if self.export_raw { "IDs NOT redacted" } else { "IDs redacted" };
                let text = format!("Exported {name} ({note}) -> {}", full.display());
                self.view.flash(&sanitize::clean_max(&text, 400), false, now);
            }
            Err(e) => self.view.flash(&format!("Export failed: {}", sanitize::clean(&e.to_string())), true, now),
        }
    }

    pub fn draw(&mut self, f: &mut Frame, now: Instant) {
        let App { store, paused, view, listen, counters, .. } = self;
        let (vstore, vnow) = match paused {
            Some((s, t)) => (&*s, *t),
            None => (&*store, now),
        };
        let [top, main, status, hints] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)])
                .areas(f.area());

        let lists = Pane::ALL.map(|p| view.visible(vstore, p));
        view.draw_tabs(f, top, &lists);
        let list = &lists[view.pane.idx()];
        match view.pane {
            Pane::Live => view.draw_live(f, main, vstore, vnow, list),
            Pane::Blips => view.draw_blips(f, main, vstore, vnow, list),
            Pane::Events => view.draw_events(f, main, vstore, vnow, list),
        }
        if let Some(addr) = view.history.clone() {
            draw_history(f, main, vstore, vnow, &addr);
        }
        let hidden = if view.noise_filter { store.rows().iter().filter(|r| r.noisy).count() } else { 0 };
        draw_status(f, status, store, counters, listen, paused.is_some(), hidden, now);
        view.draw_hints(f, hints, now);
    }
}

impl View {
    fn flash(&mut self, text: &str, error: bool, at: Instant) {
        self.message = Some(Message { text: text.to_string(), error, at });
    }

    fn shows(&self, addr: &str, noisy: bool) -> bool {
        !(self.noise_filter && noisy) && (self.filter_lc.is_empty() || addr.to_lowercase().contains(&self.filter_lc))
    }

    /// Indices into the pane's backing list, filtered and in display order.
    fn visible(&self, store: &Store, pane: Pane) -> Vec<usize> {
        match pane {
            Pane::Live => {
                let rows = store.rows();
                let mut v: Vec<usize> = (0..rows.len()).filter(|&i| self.shows(&rows[i].addr, rows[i].noisy)).collect();
                match self.sort {
                    Sort::Name => v.sort_by(|&a, &b| rows[a].addr.cmp(&rows[b].addr)),
                    Sort::LastUpdate => v.sort_by(|&a, &b| rows[b].last_update.cmp(&rows[a].last_update)),
                    Sort::Count => {
                        v.sort_by(|&a, &b| rows[b].msgs.cmp(&rows[a].msgs).then(rows[a].addr.cmp(&rows[b].addr)))
                    }
                }
                v
            }
            Pane::Blips => {
                let b = store.blips();
                (0..b.len()).rev().filter(|&i| self.shows(&b[i].addr, b[i].noisy)).collect()
            }
            Pane::Events => {
                let e = store.events();
                (0..e.len()).rev().filter(|&i| e[i].addr.as_deref().is_none_or(|a| self.shows(a, e[i].noisy))).collect()
            }
        }
    }

    fn key_of(&self, store: &Store, pane: Pane, item: usize) -> SelKey {
        match pane {
            Pane::Live => SelKey::Addr(store.rows()[item].addr.clone()),
            Pane::Blips => SelKey::Seq(store.blips()[item].seq),
            Pane::Events => SelKey::Seq(store.events()[item].seq),
        }
    }

    fn addr_of(&self, store: &Store, pane: Pane, item: usize) -> Option<Rc<str>> {
        match pane {
            Pane::Live => Some(store.rows()[item].addr.clone()),
            Pane::Blips => Some(store.blips()[item].addr.clone()),
            Pane::Events => store.events()[item].addr.clone(),
        }
    }

    /// Current selection index in `list`, following the selected item if it
    /// moved. In the newest-first panes, a selection at the top stays at the
    /// top so new entries stay in view.
    fn resolve(&mut self, store: &Store, pane: Pane, list: &[usize]) -> usize {
        if list.is_empty() {
            return 0;
        }
        let sel = &self.sel[pane.idx()];
        let follow_top = pane != Pane::Live && sel.index == 0;
        let idx = match &sel.key {
            Some(key) if !follow_top => {
                list.iter().position(|&i| &self.key_of(store, pane, i) == key).unwrap_or(sel.index.min(list.len() - 1))
            }
            _ => sel.index.min(list.len() - 1),
        };
        self.select(store, pane, list, idx);
        idx
    }

    fn select(&mut self, store: &Store, pane: Pane, list: &[usize], idx: usize) {
        let key = list.get(idx).map(|&i| self.key_of(store, pane, i));
        let sel = &mut self.sel[pane.idx()];
        sel.index = idx;
        sel.key = key;
    }

    /// Resolves the selection and scroll offset for a table body of `height`
    /// rows; returns (offset, selected index).
    fn scroll(&mut self, store: &Store, pane: Pane, list: &[usize], height: usize) -> (usize, usize) {
        let idx = self.resolve(store, pane, list);
        let height = height.max(1);
        self.page = height;
        let sel = &mut self.sel[pane.idx()];
        if idx < sel.offset {
            sel.offset = idx;
        } else if idx >= sel.offset + height {
            sel.offset = idx + 1 - height;
        }
        sel.offset = sel.offset.min(list.len().saturating_sub(height));
        (sel.offset, idx)
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect, lists: &[Vec<usize>; 3]) {
        let titles =
            [("Live", 0), ("Blips", 1), ("Events", 2)].map(|(name, i)| format!(" {name} ({}) ", lists[i].len()));
        let tabs = Tabs::new(titles)
            .select(self.pane.idx())
            .style(DIM)
            .highlight_style(Style::new().fg(Color::White).bg(Color::Blue).add_modifier(Modifier::BOLD))
            .divider("|")
            .padding("", "");
        let mut right = vec![
            Span::styled("sort: ", DIM),
            Span::raw(self.sort.label()),
            Span::styled("  noise filter: ", DIM),
            if self.noise_filter { Span::raw("on") } else { Span::styled("off", CHANGED) },
        ];
        if !self.filter.is_empty() {
            right.push(Span::styled("  filter: ", DIM));
            right.push(Span::styled(sanitize::clean(&self.filter), CHANGED));
        }
        let right = Line::from(right).right_aligned();
        let [l, r] = Layout::horizontal([Constraint::Length(46), Constraint::Min(0)]).areas(area);
        f.render_widget(tabs, l);
        f.render_widget(Paragraph::new(right), r);
    }

    fn draw_live(&mut self, f: &mut Frame, area: Rect, store: &Store, now: Instant, list: &[usize]) {
        if store.rows().is_empty() {
            let msg = vec![
                Line::raw(""),
                Line::styled("  No OSC messages yet.", HEADER),
                Line::raw(""),
                Line::raw("  Is OSC enabled in VRChat?  Action Menu -> Options -> OSC -> Enabled"),
                Line::raw("  If an expected parameter never shows up, try OSC -> Reset Config there."),
                Line::raw("  Using another port? Start VRChat with  --osc=9000:127.0.0.1:<port>"),
            ];
            f.render_widget(Paragraph::new(msg), area);
            return;
        }
        let wide = area.width >= WIDE_LAYOUT;
        let (offset, sel) = self.scroll(store, Pane::Live, list, area.height.saturating_sub(1) as usize);
        let epoch = store.epoch();

        let mut header = vec!["", "Address", "Type", "Value"];
        let mut widths =
            vec![Constraint::Length(4), Constraint::Min(20), Constraint::Length(6), Constraint::Length(18)];
        if wide {
            header.extend(["Min", "Max", "Peak 3s"]);
            widths.extend([Constraint::Length(9); 3]);
        }
        header.extend(["Age", "Hz", "Msgs", "Chg"]);
        widths.extend([Constraint::Length(6), Constraint::Length(5), Constraint::Length(7), Constraint::Length(6)]);

        let rows = list.iter().skip(offset).take(area.height as usize).map(|&i| {
            let r = &store.rows()[i];
            live_row(r, now, epoch, wide)
        });
        let table = Table::new(rows, widths)
            .header(TableRow::new(header).style(HEADER))
            .column_spacing(1)
            .row_highlight_style(SELECTED);
        let mut state = TableState::default().with_selected(Some(sel - offset));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_blips(&mut self, f: &mut Frame, area: Rect, store: &Store, now: Instant, list: &[usize]) {
        if list.is_empty() {
            let thr = store.config().blip_threshold.as_millis();
            let text = format!(
                "\n  No blips yet. A blip is a bool/int/string value that was replaced within {thr} ms\n  (change with --blip-ms). Newest appear at the top."
            );
            f.render_widget(Paragraph::new(text), area);
            return;
        }
        let (offset, sel) = self.scroll(store, Pane::Blips, list, area.height.saturating_sub(1) as usize);
        let rows = list.iter().skip(offset).take(area.height as usize).map(|&i| {
            let b = &store.blips()[i];
            let fresh = now.saturating_duration_since(b.at) < CHANGE_HIGHLIGHT;
            TableRow::new([
                Cell::from(fmt_dur(now.saturating_duration_since(b.at))),
                Cell::from(b.addr.to_string()),
                Cell::from(b.value.display()).style(if fresh { CHANGED } else { Style::new() }),
                Cell::from(fmt_dur(b.held)),
            ])
        });
        let widths = [Constraint::Length(8), Constraint::Min(20), Constraint::Length(24), Constraint::Length(8)];
        let table = Table::new(rows, widths)
            .header(TableRow::new(["Ago", "Address", "Value (short-lived)", "Held"]).style(HEADER))
            .column_spacing(1)
            .row_highlight_style(SELECTED);
        let mut state = TableState::default().with_selected(Some(sel - offset));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_events(&mut self, f: &mut Frame, area: Rect, store: &Store, now: Instant, list: &[usize]) {
        if list.is_empty() {
            f.render_widget(
                Paragraph::new("\n  No events yet (avatar changes and first sightings of addresses show up here)."),
                area,
            );
            return;
        }
        let (offset, sel) = self.scroll(store, Pane::Events, list, area.height.saturating_sub(1) as usize);
        let rows = list.iter().skip(offset).take(area.height as usize).map(|&i| {
            let e = &store.events()[i];
            let addr = e.addr.as_deref().unwrap_or("");
            let (kind, detail, style) = match &e.kind {
                EventKind::AvatarChange { id } => {
                    ("AVATAR", format!("{addr} = {id}"), Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD))
                }
                EventKind::NewAddress { ty } => ("new", format!("{addr}  ({ty})"), Style::new()),
                EventKind::AddressLimit => (
                    "LIMIT",
                    format!("address limit ({}) reached; new addresses are ignored", crate::state::MAX_ADDRESSES),
                    Style::new().fg(Color::Red),
                ),
            };
            TableRow::new([
                Cell::from(fmt_dur(now.saturating_duration_since(e.at))),
                Cell::from(kind),
                Cell::from(detail),
            ])
            .style(style)
        });
        let widths = [Constraint::Length(8), Constraint::Length(6), Constraint::Min(20)];
        let table = Table::new(rows, widths)
            .header(TableRow::new(["Ago", "Event", "Detail"]).style(HEADER))
            .column_spacing(1)
            .row_highlight_style(SELECTED);
        let mut state = TableState::default().with_selected(Some(sel - offset));
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_hints(&self, f: &mut Frame, area: Rect, now: Instant) {
        let line = if self.confirm_clear {
            Line::styled(
                " Clear ALL recorded rows, blips and events?  y = yes, any other key = cancel ",
                Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else if self.editing_filter {
            Line::from(vec![
                Span::styled(" Filter address: ", HEADER),
                Span::raw(sanitize::clean(&self.filter)),
                Span::styled("_", Style::new().add_modifier(Modifier::SLOW_BLINK)),
                Span::styled("   (Enter keep, Esc clear)", DIM),
            ])
        } else if let Some(m) = self.message.as_ref().filter(|m| now.saturating_duration_since(m.at) < MESSAGE_TIME) {
            let style = if m.error { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Green) };
            Line::styled(format!(" {}", m.text), style)
        } else if self.history.is_some() {
            Line::styled(" Enter/Esc close history   Up/Down previous/next row   q quit", DIM)
        } else {
            Line::styled(
                " q quit  p pause  / filter  s sort  Tab pane  Up/Down select  Enter history  n noise  c clear  e export",
                DIM,
            )
        };
        f.render_widget(Paragraph::new(line), area);
    }
}

fn live_row(r: &Row, now: Instant, epoch: u64, wide: bool) -> TableRow<'static> {
    let age = now.saturating_duration_since(r.last_update);
    let prev = r.is_previous_avatar(epoch);
    let base = if prev || age >= STALE_AFTER { DIM } else { Style::new() };
    let changed = now.saturating_duration_since(r.value_since) < CHANGE_HIGHLIGHT;
    let value_style = match &r.value {
        _ if changed => CHANGED,
        Value::Bool(true) if base == Style::new() => Style::new().fg(Color::Green),
        _ => base,
    };

    let mut cells = vec![
        Cell::from(if prev { "prev" } else { "" }),
        Cell::from(r.addr.to_string()),
        Cell::from(r.ty.to_string()),
        Cell::from(r.value.display()).style(value_style),
    ];
    if wide {
        let (min, max) =
            r.float.as_ref().and_then(|s| s.range()).map_or((String::new(), String::new()), |(a, b)| {
                (crate::state::fmt_float(a), crate::state::fmt_float(b))
            });
        let peak = r.peak(now).map(crate::state::fmt_float).unwrap_or_default();
        cells.extend([Cell::from(min), Cell::from(max), Cell::from(peak)]);
    }
    cells.extend([
        Cell::from(fmt_dur(age)),
        Cell::from(fmt_hz(r.hz(now))),
        Cell::from(r.msgs.to_string()),
        Cell::from(r.changes.to_string()),
    ]);
    TableRow::new(cells).style(base)
}

#[allow(clippy::too_many_arguments)]
fn draw_status(
    f: &mut Frame,
    area: Rect,
    store: &Store,
    counters: &Counters,
    listen: &str,
    paused: bool,
    hidden: usize,
    now: Instant,
) {
    let bar = Style::new().fg(Color::White).bg(Color::DarkGray);
    let sep = Span::styled(" | ", bar.fg(Color::Gray));
    let mut spans = vec![Span::styled(format!(" UDP {listen}"), bar.add_modifier(Modifier::BOLD))];
    let dropped = counters.queue_full() + counters.non_loopback();
    if store.stats.packets == 0 && dropped == 0 {
        spans.push(sep.clone());
        spans.push(Span::styled(
            "waiting... nothing received yet. Is OSC enabled in VRChat? (Action Menu -> Options -> OSC -> Enabled)",
            bar.fg(Color::Yellow),
        ));
    } else {
        let mut dropped_text = format!("dropped {dropped}");
        if counters.non_loopback() > 0 {
            dropped_text.push_str(&format!(" ({} non-local)", counters.non_loopback()));
        }
        let items = [
            format!("{} msg/s", fmt_hz(store.rate(now))),
            format!("total {}", store.stats.messages),
            dropped_text,
            format!("malformed {}", store.stats.malformed),
            format!("{} addresses", store.rows().len()),
            format!("{hidden} hidden"),
        ];
        for item in items {
            spans.push(sep.clone());
            spans.push(Span::styled(item, bar));
        }
        if store.stats.over_cap > 0 {
            spans.push(sep.clone());
            spans.push(Span::styled(
                format!("address limit: {} msgs ignored", store.stats.over_cap),
                bar.fg(Color::LightRed),
            ));
        }
    }
    if paused {
        spans.push(Span::styled(" ", bar));
        spans.push(Span::styled(" PAUSED ", Style::new().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn draw_history(f: &mut Frame, area: Rect, store: &Store, now: Instant, addr: &str) {
    let popup = inset(area, 2, 1);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(Line::styled(format!(" History: {addr} "), HEADER))
        .title_bottom(Line::styled(" Enter/Esc close  Up/Down previous/next row ", DIM));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let Some(r) = store.row(addr) else {
        f.render_widget(Paragraph::new("No data for this address (it was cleared)."), inner);
        return;
    };
    let is_float = r.float.is_some();

    let mut info = vec![Line::from(vec![
        Span::styled("type ", DIM),
        Span::raw(r.ty.to_string()),
        Span::styled("   value ", DIM),
        Span::styled(r.value.display(), CHANGED),
        Span::styled("   held ", DIM),
        Span::raw(fmt_dur(now.saturating_duration_since(r.value_since))),
        Span::styled("   msgs ", DIM),
        Span::raw(r.msgs.to_string()),
        Span::styled("   changes ", DIM),
        Span::raw(r.changes.to_string()),
        Span::styled("   ", DIM),
        Span::raw(fmt_hz(r.hz(now))),
        Span::styled(" Hz", DIM),
    ])];
    let mut second = vec![
        Span::styled("first seen ", DIM),
        Span::raw(fmt_dur(now.saturating_duration_since(r.first_seen))),
        Span::styled(" ago   last update ", DIM),
        Span::raw(fmt_dur(now.saturating_duration_since(r.last_update))),
        Span::styled(" ago", DIM),
    ];
    if r.is_previous_avatar(store.epoch()) {
        second.push(Span::styled("   last seen under a previous avatar", Style::new().fg(Color::Magenta)));
    }
    info.push(Line::from(second));
    if let Some(stats) = &r.float {
        let (min, max) = stats
            .range()
            .map_or(("-".into(), "-".into()), |(a, b)| (crate::state::fmt_float(a), crate::state::fmt_float(b)));
        let peak = r.peak(now).map_or("-".into(), crate::state::fmt_float);
        info.push(Line::from(vec![
            Span::styled("session min ", DIM),
            Span::raw(min),
            Span::styled("   max ", DIM),
            Span::raw(max),
            Span::styled("   peak (last ~3 s) ", DIM),
            Span::raw(peak),
        ]));
    }

    let [info_area, spark_area, tables] = Layout::vertical([
        Constraint::Length(info.len() as u16 + 1),
        Constraint::Length(if is_float { 5 } else { 0 }),
        Constraint::Min(3),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(info), info_area);

    if is_float {
        let block = Block::bordered().title(Line::styled(" recent values (oldest left) ", DIM));
        let width = block.inner(spark_area).width as usize;
        let values: Vec<f64> = r.history.iter().filter_map(|c| c.value.as_float()).filter(|v| v.is_finite()).collect();
        let values = &values[values.len().saturating_sub(width)..];
        let (lo, hi) = values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        let data: Vec<u64> = values
            .iter()
            .map(|&v| if hi - lo > f64::EPSILON { 5 + ((v - lo) / (hi - lo) * 95.0) as u64 } else { 50 })
            .collect();
        let spark = Sparkline::default().block(block).data(&data).max(100).style(Style::new().fg(Color::Cyan));
        f.render_widget(spark, spark_area);
    }

    let [left, right] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(tables);

    let changes = r.history.iter().rev().map(|c| {
        let held = match c.held {
            Some(d) => fmt_dur(d),
            None => format!("{} (now)", fmt_dur(now.saturating_duration_since(c.at))),
        };
        TableRow::new([fmt_dur(now.saturating_duration_since(c.at)), c.value.display(), held])
    });
    let changes = Table::new(changes, [Constraint::Length(8), Constraint::Min(10), Constraint::Length(14)])
        .header(TableRow::new(["Ago", "Value", "Held"]).style(HEADER))
        .block(Block::bordered().title(format!(" Last {} changes, newest first ", r.history.len())));
    f.render_widget(changes, left);

    let mut distinct: Vec<&(Value, u64)> = r.distinct.iter().collect();
    distinct.sort_by(|a, b| b.1.cmp(&a.1));
    let mut rows: Vec<TableRow> = distinct.iter().map(|(v, n)| TableRow::new([v.display(), n.to_string()])).collect();
    if r.distinct_overflow > 0 {
        rows.push(TableRow::new([String::from("(other values)"), r.distinct_overflow.to_string()]).style(DIM));
    }
    let distinct = Table::new(rows, [Constraint::Min(10), Constraint::Length(8)])
        .header(TableRow::new(["Value", "Msgs"]).style(HEADER))
        .block(Block::bordered().title(format!(" Distinct values ({}) ", r.distinct.len())));
    f.render_widget(distinct, right);
}

fn inset(r: Rect, dx: u16, dy: u16) -> Rect {
    let w = r.width.saturating_sub(dx * 2);
    let h = r.height.saturating_sub(dy * 2);
    Rect { x: r.x + (r.width - w) / 2, y: r.y + (r.height - h) / 2, width: w, height: h }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{}ms", d.as_millis())
    } else if s < 10.0 {
        format!("{s:.2}s")
    } else if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    } else {
        format!("{}h{:02}m", d.as_secs() / 3600, (d.as_secs() / 60) % 60)
    }
}

fn fmt_hz(hz: f64) -> String {
    if hz < 0.05 {
        "0".into()
    } else if hz < 10.0 {
        format!("{hz:.1}")
    } else {
        format!("{hz:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AVATAR_CHANGE, Config, TypeName};
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = App::new(Store::new(Config::default(), t0), "127.0.0.1:9001".into(), Arc::default(), false);
        let s = app.store_mut();
        let ms = Duration::from_millis;
        s.apply(AVATAR_CHANGE, TypeName::Borrowed("string"), Value::Str("avtr_a".into()), t0);
        s.apply("/avatar/parameters/OldOnly", TypeName::Borrowed("int"), Value::Int(3), t0 + ms(5));
        s.apply(AVATAR_CHANGE, TypeName::Borrowed("string"), Value::Str("avtr_b".into()), t0 + ms(1000));
        for i in 0..120u64 {
            let v = (i as f64 / 10.0).sin() * if i == 60 { 4.0 } else { 1.0 };
            s.apply(
                "/avatar/parameters/VelocityX",
                TypeName::Borrowed("float"),
                Value::Float(v),
                t0 + ms(1000 + i * 16),
            );
            s.apply("/avatar/parameters/Custom", TypeName::Borrowed("float"), Value::Float(v), t0 + ms(1000 + i * 16));
        }
        s.apply("/avatar/parameters/Blink", TypeName::Borrowed("bool"), Value::Bool(false), t0 + ms(1100));
        s.apply("/avatar/parameters/Blink", TypeName::Borrowed("bool"), Value::Bool(true), t0 + ms(2000));
        s.apply("/avatar/parameters/Blink", TypeName::Borrowed("bool"), Value::Bool(false), t0 + ms(2050));
        s.ingest_packet(b"garbage", t0 + ms(2100));
        (app, t0 + ms(3000))
    }

    fn render(app: &mut App, now: Instant, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f, now)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(app: &mut App, code: KeyCode, now: Instant) {
        app.on_key(KeyEvent::from(code), now);
    }

    #[test]
    fn renders_all_panes_and_history() {
        let (mut app, now) = app();
        let live = render(&mut app, now, 130, 20);
        assert!(live.contains("/avatar/parameters/Blink"), "{live}");
        assert!(live.contains("prev") && live.contains("OldOnly"), "{live}");
        assert!(!live.contains("VelocityX"), "noise filter on by default:\n{live}");
        assert!(live.contains("1 hidden") && live.contains("malformed 1"), "{live}");

        key(&mut app, KeyCode::Char('n'), now);
        assert!(render(&mut app, now, 130, 20).contains("VelocityX"));
        render(&mut app, now, 60, 12); // narrow terminal must not panic

        key(&mut app, KeyCode::Tab, now);
        let blips = render(&mut app, now, 130, 20);
        assert!(blips.contains("Blink") && blips.contains("50ms"), "{blips}");
        key(&mut app, KeyCode::Tab, now);
        let events = render(&mut app, now, 130, 20);
        assert!(events.contains("AVATAR") && events.contains("avtr_b"), "{events}");

        key(&mut app, KeyCode::Tab, now);
        key(&mut app, KeyCode::Char('/'), now);
        for c in "custom".chars() {
            key(&mut app, KeyCode::Char(c), now);
        }
        key(&mut app, KeyCode::Enter, now);
        key(&mut app, KeyCode::Enter, now);
        let hist = render(&mut app, now, 130, 30);
        assert!(hist.contains("History: /avatar/parameters/Custom") && hist.contains("peak (last ~3 s)"), "{hist}");

        key(&mut app, KeyCode::Char('p'), now);
        assert!(render(&mut app, now, 130, 20).contains("PAUSED"));
        key(&mut app, KeyCode::Char('c'), now);
        key(&mut app, KeyCode::Char('y'), now);
        assert!(app.store.rows().is_empty());
        key(&mut app, KeyCode::Char('q'), now);
        assert!(app.quit);
    }
}
