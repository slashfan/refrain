//! The application state and its reaction to events.
//!
//! A deliberate split of roles: this module **decides**, `ui.rs` merely
//! **draws**. The sorted tables are recomputed once per clock tick (and not on
//! every frame), which keeps rendering nearly free even when the logs scroll at
//! 100,000 lines per second.

use crate::cli::Cli;
use crate::event::Event;
use crate::export;
use crate::parser::Level;
use crate::stats::{DeprecationStat, ErrorStat, HttpStat, NPlusOne, Stats, StreamEntry};
use chrono::{DateTime, FixedOffset};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a transient message stays up — "written to …". Long enough to be
/// read, short enough not to clutter the banner.
const FLASH: Duration = Duration::from_secs(4);

/// Number of rows kept in the sorted tables. Beyond that nobody scrolls: may
/// as well not pay for the sort.
const MAX_ROWS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Errors,
    Endpoints,
    Sql,
    Outbound,
    Deprecations,
    Stream,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Overview,
        Tab::Errors,
        Tab::Endpoints,
        Tab::Sql,
        Tab::Outbound,
        Tab::Deprecations,
        Tab::Stream,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Errors => "Errors",
            Tab::Endpoints => "Endpoints",
            Tab::Sql => "SQL",
            // Not "HTTP": every tab here is about HTTP. What sets these
            // apart is the direction — calls this application makes.
            Tab::Outbound => "Outbound",
            Tab::Deprecations => "Deprecations",
            Tab::Stream => "Stream",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RouteSort {
    P95,
    Requests,
    Errors,
    Max,
}

impl RouteSort {
    pub fn label(self) -> &'static str {
        match self {
            RouteSort::P95 => "p95",
            RouteSort::Requests => "requests",
            RouteSort::Errors => "errors",
            RouteSort::Max => "max",
        }
    }

    fn next(self) -> Self {
        match self {
            RouteSort::P95 => RouteSort::Max,
            RouteSort::Max => RouteSort::Requests,
            RouteSort::Requests => RouteSort::Errors,
            RouteSort::Errors => RouteSort::P95,
        }
    }
}

/// One row of the error table. Only what the table displays is copied here;
/// the detail (full message, context) is reread from `Stats` at render time, so
/// as not to duplicate large blocks of text on every tick.
pub struct ErrorRow {
    pub signature: String,
    pub count: u64,
    pub level: Level,
    pub channel: String,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

/// An N+1 pattern as shown in the table.
pub struct NPlusOneRow {
    pub key: (String, u64),
    pub endpoint: String,
    pub max_count: u32,
    pub avg_count: f32,
    pub requests: u64,
    pub sql: String,
}

/// One row of the outbound-call table.
pub struct OutboundRow {
    pub key: u64,
    pub shape: String,
    pub calls: u64,
    pub timed: u64,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
    /// Calls that carried a status, and those that came back 4xx or 5xx: the
    /// denominator travels with the counters, otherwise "0" and "the line said
    /// nothing" would look alike on screen.
    pub responses: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub max_per_request: u32,
    pub worst_endpoint: Option<String>,
}

/// One row of the deprecation table.
pub struct DeprecationRow {
    pub key: (String, String),
    pub count: u64,
    /// The route that triggered it last, when one is known.
    pub endpoint: Option<String>,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

pub struct RouteRow {
    pub name: String,
    pub requests: u64,
    /// Responses carrying a status, and those in 5xx: the denominator travels
    /// with the counter, otherwise "0" and "no status read" would look alike on
    /// screen.
    pub responses: u64,
    pub status_5xx: u64,
    pub avg_queries: f32,
    pub avg_calls: f32,
    pub errors: u64,
    pub timed: u64,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
    pub error_rate: f32,
}

pub struct App {
    pub cli: Cli,
    pub stats: Stats,
    pub tab: Tab,
    pub error_rows: Vec<ErrorRow>,
    pub route_rows: Vec<RouteRow>,
    pub nplus1_rows: Vec<NPlusOneRow>,
    pub outbound_rows: Vec<OutboundRow>,
    pub deprecation_rows: Vec<DeprecationRow>,
    pub error_sel: usize,
    pub route_sel: usize,
    pub nplus1_sel: usize,
    pub outbound_sel: usize,
    pub deprecation_sel: usize,
    /// Stream offset from the bottom. 0 = stuck to the latest lines.
    pub stream_offset: usize,
    pub frozen: bool,
    pub min_level: Level,
    /// The endpoint being followed, if there is one. It filters the errors,
    /// the N+1 patterns and the stream — not the endpoint table, since that is
    /// where it is chosen.
    pub focus: Option<String>,
    /// Stream search pattern. Empty: no filter.
    pub search: String,
    /// The pattern is being typed. As long as it is, keys feed the pattern
    /// instead of triggering the shortcuts.
    pub searching: bool,
    pub route_sort: RouteSort,
    pub show_help: bool,
    pub should_quit: bool,
    pub sources_total: usize,
    pub sources_done: usize,
    pub failures: Vec<String>,
    pub started: Instant,
    /// Transient banner message, with the instant it fades at.
    flash: Option<(String, Instant)>,
    /// Entries have arrived since the tables were last rebuilt.
    dirty: bool,
}

impl App {
    pub fn new(cli: Cli, sources_total: usize) -> Self {
        let stats = Stats::new(&cli);
        let min_level = cli.min_level;
        Self {
            cli,
            stats,
            tab: Tab::Overview,
            error_rows: Vec::new(),
            route_rows: Vec::new(),
            nplus1_rows: Vec::new(),
            outbound_rows: Vec::new(),
            deprecation_rows: Vec::new(),
            error_sel: 0,
            route_sel: 0,
            nplus1_sel: 0,
            outbound_sel: 0,
            deprecation_sel: 0,
            stream_offset: 0,
            frozen: false,
            min_level,
            focus: None,
            search: String::new(),
            searching: false,
            route_sort: RouteSort::P95,
            show_help: false,
            should_quit: false,
            sources_total,
            sources_done: 0,
            failures: Vec::new(),
            started: Instant::now(),
            flash: None,
            dirty: true,
        }
    }

    pub fn all_sources_done(&self) -> bool {
        self.sources_done >= self.sources_total
    }

    /// Handles one event. Returns `true` if a redraw is needed.
    pub fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Batch { source, entries } => {
                for entry in entries {
                    self.stats.ingest(source, entry);
                }
                self.dirty = true;
                // No redraw: we wait for the Tick. Otherwise a fast stream
                // would spend its time repainting the screen instead of counting.
                false
            }
            Event::Skipped(n) => {
                self.stats.skipped += n;
                false
            }
            Event::CaughtUp(source) => {
                self.stats.source_caught_up(source);
                false
            }
            Event::SourceDone(source) => {
                self.sources_done += 1;
                self.stats.source_done(source);
                // Above all no `finalize` while another source is still
                // reading: it closes every open request, including those whose
                // lines sleep in a file we have not finished walking.
                if self.all_sources_done() {
                    self.stats.finalize();
                }
                self.dirty = true;
                true
            }
            Event::Failed(message) => {
                self.failures.push(message);
                true
            }
            Event::Key(key) => {
                self.on_key(key);
                true
            }
            Event::Resize => true,
            Event::Tick => {
                // Nothing has arrived since the last tick: the requests still
                // open would stay so indefinitely, for want of a new line to
                // advance the log clock.
                if !self.dirty && self.stats.sweep_idle() > 0 {
                    self.dirty = true;
                }
                if self.dirty {
                    self.refresh_views();
                    self.dirty = false;
                }
                true
            }
        }
    }

    /// Rebuilds the sorted tables from the raw counters.
    fn refresh_views(&mut self) {
        // -- errors, sorted by frequency -----------------------------------
        let mut errors: Vec<(&String, &ErrorStat)> = self.stats.errors.iter().collect();
        // The name always breaks ties: a hash map does not enumerate twice in
        // the same order, and two rows tying would jump from one clock tick to
        // the next under the cursor.
        errors.sort_unstable_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| b.1.level.cmp(&a.1.level))
                .then_with(|| a.0.cmp(b.0))
        });
        self.error_rows = errors
            .into_iter()
            .filter(|(_, stat)| self.shows_endpoint(stat.endpoint.as_deref()))
            .take(MAX_ROWS)
            .map(|(signature, stat)| ErrorRow {
                signature: signature.clone(),
                count: stat.count,
                level: stat.level,
                channel: stat.channel.clone(),
                last_seen: stat.last_seen,
            })
            .collect();

        // -- endpoints -----------------------------------------------------
        let mut rows: Vec<RouteRow> = self
            .stats
            .routes
            .iter()
            .map(|(name, route)| {
                let quantiles = route.quantiles();
                RouteRow {
                    name: name.clone(),
                    requests: route.requests.max(route.timed),
                    responses: route.responses,
                    status_5xx: route.status_5xx,
                    avg_queries: route.avg_queries(),
                    avg_calls: route.avg_calls(),
                    errors: route.errors,
                    timed: route.timed,
                    p50: quantiles.p50,
                    p95: quantiles.p95,
                    max: route.max_ms,
                    error_rate: route.error_rate(),
                }
            })
            .collect();

        match self.route_sort {
            RouteSort::P95 => rows
                .sort_unstable_by(|a, b| b.p95.total_cmp(&a.p95).then_with(|| a.name.cmp(&b.name))),
            RouteSort::Max => rows
                .sort_unstable_by(|a, b| b.max.total_cmp(&a.max).then_with(|| a.name.cmp(&b.name))),
            RouteSort::Requests => rows.sort_unstable_by(|a, b| {
                b.requests
                    .cmp(&a.requests)
                    .then_with(|| a.name.cmp(&b.name))
            }),
            RouteSort::Errors => rows
                .sort_unstable_by(|a, b| b.errors.cmp(&a.errors).then_with(|| a.name.cmp(&b.name))),
        }
        rows.truncate(MAX_ROWS);
        self.route_rows = rows;

        // -- N+1 patterns, worst to least bad ------------------------------
        let mut nplus1: Vec<(&(String, u64), &NPlusOne)> = self.stats.nplus1.iter().collect();
        nplus1.sort_unstable_by(|a, b| {
            b.1.max_count
                .cmp(&a.1.max_count)
                .then_with(|| b.1.requests.cmp(&a.1.requests))
                .then_with(|| a.0.cmp(b.0))
        });
        self.nplus1_rows = nplus1
            .into_iter()
            .filter(|(_, needle)| self.shows_endpoint(Some(needle.endpoint.as_str())))
            .take(MAX_ROWS)
            .map(|(key, needle)| NPlusOneRow {
                key: key.clone(),
                endpoint: needle.endpoint.clone(),
                max_count: needle.max_count,
                avg_count: needle.avg_count(),
                requests: needle.requests,
                sql: needle.sql.clone(),
            })
            .collect();

        // -- outbound calls, worst latency first ---------------------------
        // The follow does not apply here: a shape is a provider, and the same
        // provider is called from several endpoints. The row names the
        // endpoint that calls it most within one request instead, and `Enter`
        // goes there.
        let mut outbound: Vec<(&u64, &HttpStat)> = self.stats.http.iter().collect();
        outbound.sort_unstable_by(|a, b| {
            b.1.quantiles()
                .p95
                .total_cmp(&a.1.quantiles().p95)
                .then_with(|| b.1.calls.cmp(&a.1.calls))
                .then_with(|| a.1.shape.cmp(&b.1.shape))
        });
        self.outbound_rows = outbound
            .into_iter()
            .take(MAX_ROWS)
            .map(|(key, shape)| {
                let quantiles = shape.quantiles();
                OutboundRow {
                    key: *key,
                    shape: shape.shape.clone(),
                    calls: shape.calls,
                    timed: shape.timed,
                    p50: quantiles.p50,
                    p95: quantiles.p95,
                    max: shape.max_ms,
                    responses: shape.responses,
                    status_4xx: shape.status_4xx,
                    status_5xx: shape.status_5xx,
                    max_per_request: shape.max_per_request,
                    worst_endpoint: shape.worst_endpoint.clone(),
                }
            })
            .collect();

        // -- deprecations, most frequent first ------------------------------
        let mut deprecations: Vec<(&(String, String), &DeprecationStat)> =
            self.stats.deprecations.iter().collect();
        deprecations.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        self.deprecation_rows = deprecations
            .into_iter()
            .filter(|(_, stat)| self.shows_endpoint(stat.endpoint.as_deref()))
            .take(MAX_ROWS)
            .map(|(key, stat)| DeprecationRow {
                key: key.clone(),
                count: stat.count,
                endpoint: stat.endpoint.clone(),
                last_seen: stat.last_seen,
            })
            .collect();

        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        self.error_sel = self.error_sel.min(self.error_rows.len().saturating_sub(1));
        self.route_sel = self.route_sel.min(self.route_rows.len().saturating_sub(1));
        self.nplus1_sel = self
            .nplus1_sel
            .min(self.nplus1_rows.len().saturating_sub(1));
        self.outbound_sel = self
            .outbound_sel
            .min(self.outbound_rows.len().saturating_sub(1));
        self.deprecation_sel = self
            .deprecation_sel
            .min(self.deprecation_rows.len().saturating_sub(1));
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C must always exit, whatever screen is showing.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return;
        }
        // While a pattern is being typed, every character goes to it: without
        // that detour, searching for "queue" would quit on the `q`.
        if self.searching {
            self.on_search_key(key);
            return;
        }
        if self.show_help {
            // Any key closes the help.
            self.show_help = false;
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                return;
            }
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            // Esc undoes what is active, narrowest to widest, and only quits
            // when there is nothing left to undo. Without that gradation, a
            // filter set by mistake could only be lifted by restarting the
            // program.
            KeyCode::Esc => self.escape(),
            KeyCode::Enter => self.toggle_focus(),
            KeyCode::Char('?') | KeyCode::Char('h') => self.show_help = true,

            KeyCode::Tab | KeyCode::Right => self.cycle_tab(1),
            KeyCode::BackTab | KeyCode::Left => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='7') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
            }

            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home | KeyCode::Char('g') => self.jump_start(),
            KeyCode::End | KeyCode::Char('G') => self.jump_end(),

            KeyCode::Char(' ') => self.frozen = !self.frozen,
            KeyCode::Char('s') => {
                self.route_sort = self.route_sort.next();
                self.dirty = true;
            }
            KeyCode::Char('r') => {
                let cli = self.cli.clone();
                self.stats.reset(&cli);
                self.error_rows.clear();
                self.route_rows.clear();
                self.nplus1_rows.clear();
                self.outbound_rows.clear();
                self.deprecation_rows.clear();
                self.started = Instant::now();
            }
            KeyCode::Char('/') => {
                // The pattern filters the stream only: may as well take whoever
                // is searching there, from any tab.
                self.tab = Tab::Stream;
                self.searching = true;
            }
            KeyCode::Char('w') => self.export_to_file(),
            KeyCode::Char('y') => self.export_to_clipboard(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.shift_min_level(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.shift_min_level(-1),
            _ => {}
        }
    }

    /// Writes the selected item to a file in the current directory. Depends on
    /// nothing, and therefore works at the end of an `ssh`, where the local
    /// clipboard is out of reach.
    fn export_to_file(&mut self) {
        let message = match export::write_to(self, Path::new(".")) {
            Ok(path) => format!("written to {}", path.display()),
            Err(err) => format!("failed: {err}"),
        };
        self.set_flash(message);
    }

    /// Asks the terminal to put the selected item in the clipboard — see
    /// `export::clipboard_sequence` for why it is done this way.
    fn export_to_clipboard(&mut self) {
        let report = export::report(self);
        let sequence = export::clipboard_sequence(&report.text);
        let mut out = std::io::stdout();
        let message = match out
            .write_all(sequence.as_bytes())
            .and_then(|()| out.flush())
        {
            // The terminal answers nothing: we cannot know whether it really
            // honoured the request, only that it went out.
            Ok(()) => format!("{} bytes sent to the clipboard", report.text.len()),
            Err(err) => format!("copy failed: {err}"),
        };
        self.set_flash(message);
    }

    fn set_flash(&mut self, message: String) {
        self.flash = Some((message, Instant::now() + FLASH));
    }

    /// The transient message, as long as it has not expired. Expiry is read at
    /// draw time rather than scheduled: a clock tick comes round every 250 ms
    /// anyway.
    pub fn flash(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(message, _)| message.as_str())
    }

    /// Follows — or stops following — the selected endpoint. From the SQL tab,
    /// it is the endpoint of the N+1 pattern: that is where the culprit is
    /// discovered, and one wants to see what else it does right away. From
    /// the Outbound tab, the endpoint that calls that provider most within one
    /// request. From the Deprecations tab, the route that triggered it last.
    fn toggle_focus(&mut self) {
        let picked = match self.tab {
            Tab::Endpoints => self.route_rows.get(self.route_sel).map(|r| r.name.clone()),
            Tab::Sql => self
                .nplus1_rows
                .get(self.nplus1_sel)
                .map(|r| r.endpoint.clone()),
            Tab::Outbound => self
                .outbound_rows
                .get(self.outbound_sel)
                .and_then(|r| r.worst_endpoint.clone()),
            Tab::Deprecations => self
                .deprecation_rows
                .get(self.deprecation_sel)
                .and_then(|r| r.endpoint.clone()),
            _ => return,
        };
        let Some(picked) = picked else { return };
        // The same key twice on the same row: release.
        self.focus = if self.focus.as_deref() == Some(picked.as_str()) {
            None
        } else {
            Some(picked)
        };
        self.on_filter_changed();
    }

    fn escape(&mut self) {
        if !self.search.is_empty() {
            self.search.clear();
        } else if self.focus.is_some() {
            self.focus = None;
        } else {
            self.should_quit = true;
            return;
        }
        self.on_filter_changed();
    }

    /// The visible lists have just changed: rebuild them and start from the
    /// top. Keeping the selection would aim at a row that no longer exists in
    /// the reduced list.
    ///
    /// The rebuild is immediate, and not deferred to the next clock tick: a
    /// keystroke must show at once, not a quarter of a second later.
    fn on_filter_changed(&mut self) {
        self.error_sel = 0;
        self.nplus1_sel = 0;
        self.deprecation_sel = 0;
        self.stream_offset = 0;
        self.refresh_views();
    }

    /// Does this endpoint pass the follow in force? A line with no known
    /// endpoint does not pass: we cannot claim it belongs to the one being
    /// followed.
    fn shows_endpoint(&self, endpoint: Option<&str>) -> bool {
        match &self.focus {
            None => true,
            Some(focus) => endpoint == Some(focus.as_str()),
        }
    }

    /// The keys while a pattern is being typed. `Enter` confirms and hands
    /// back to the shortcuts, `Esc` clears the pattern — the only way back to
    /// the whole stream, and what keeps a forgotten filter from looking like
    /// logs having gone quiet.
    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search.clear();
                self.searching = false;
            }
            KeyCode::Enter => self.searching = false,
            KeyCode::Backspace => {
                self.search.pop();
            }
            KeyCode::Char(c) => self.search.push(c),
            _ => return,
        }
        // The visible lines have just changed: stick back to the present,
        // otherwise we scroll through a history that no longer exists.
        self.stream_offset = 0;
    }

    /// Does this entry belong in the stream? The minimum level, and the search
    /// pattern if there is one.
    ///
    /// This is where the decision is made, not in the rendering: `ui.rs` draws
    /// what it is given.
    pub fn stream_shows(&self, item: &StreamEntry) -> bool {
        let entry = &item.entry;
        if entry.level < self.min_level {
            return false;
        }
        if !self.shows_endpoint(item.endpoint.as_deref()) {
            return false;
        }
        if self.search.is_empty() {
            return true;
        }
        // The channel and the route count as much as the message: one searches
        // for "doctrine" as readily as "app_login" or "Connection refused".
        contains_ignore_case(&entry.message, &self.search)
            || contains_ignore_case(&entry.channel, &self.search)
            || entry
                .route()
                .is_some_and(|route| contains_ignore_case(route, &self.search))
            || entry
                .request_uri()
                .is_some_and(|uri| contains_ignore_case(uri, &self.search))
            || item
                .endpoint
                .as_deref()
                .is_some_and(|name| contains_ignore_case(name, &self.search))
    }

    fn cycle_tab(&mut self, delta: isize) {
        let count = Tab::ALL.len() as isize;
        let index = (self.tab.index() as isize + delta).rem_euclid(count);
        self.tab = Tab::ALL[index as usize];
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            Tab::Errors => {
                self.error_sel = step(self.error_sel, delta, self.error_rows.len());
            }
            Tab::Endpoints => {
                self.route_sel = step(self.route_sel, delta, self.route_rows.len());
            }
            Tab::Sql => {
                self.nplus1_sel = step(self.nplus1_sel, delta, self.nplus1_rows.len());
            }
            Tab::Outbound => {
                self.outbound_sel = step(self.outbound_sel, delta, self.outbound_rows.len());
            }
            Tab::Deprecations => {
                self.deprecation_sel =
                    step(self.deprecation_sel, delta, self.deprecation_rows.len());
            }
            Tab::Stream => {
                // In the stream, "down" moves towards the present: the offset
                // counts from the bottom, so it decreases.
                let max = self.stats.recent.len();
                let next = self.stream_offset as isize - delta;
                self.stream_offset = next.clamp(0, max as isize) as usize;
                // Going back into history naturally freezes the display.
                self.frozen = self.stream_offset > 0;
            }
            Tab::Overview => {}
        }
    }

    fn jump_start(&mut self) {
        match self.tab {
            Tab::Errors => self.error_sel = 0,
            Tab::Endpoints => self.route_sel = 0,
            Tab::Sql => self.nplus1_sel = 0,
            Tab::Outbound => self.outbound_sel = 0,
            Tab::Deprecations => self.deprecation_sel = 0,
            Tab::Stream => {
                self.stream_offset = self.stats.recent.len();
                self.frozen = true;
            }
            Tab::Overview => {}
        }
    }

    fn jump_end(&mut self) {
        match self.tab {
            Tab::Errors => self.error_sel = self.error_rows.len().saturating_sub(1),
            Tab::Endpoints => self.route_sel = self.route_rows.len().saturating_sub(1),
            Tab::Sql => self.nplus1_sel = self.nplus1_rows.len().saturating_sub(1),
            Tab::Outbound => self.outbound_sel = self.outbound_rows.len().saturating_sub(1),
            Tab::Deprecations => {
                self.deprecation_sel = self.deprecation_rows.len().saturating_sub(1);
            }
            Tab::Stream => {
                self.stream_offset = 0;
                self.frozen = false;
            }
            Tab::Overview => {}
        }
    }

    fn shift_min_level(&mut self, delta: isize) {
        let index = (self.min_level.index() as isize + delta).clamp(0, 7) as usize;
        self.min_level = Level::ALL[index];
    }

    /// Instant throughput, in lines per second, averaged over 5 s.
    pub fn rate(&self) -> f64 {
        self.stats.timeline.rate(5)
    }
}

/// `contains`, case-insensitive and allocation-free.
///
/// The obvious solution — `haystack.to_lowercase().contains(&needle.to_lowercase())`
/// — would copy every message on every frame. So we compare character by
/// character, lowercasing on the fly. `to_lowercase` returns an iterator because
/// one lowercase form can span several characters ("İ" gives two); `flat_map`
/// chains them on both sides, which keeps the comparison correct.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.char_indices().any(|(start, _)| {
        let mut haystack = haystack[start..].chars().flat_map(char::to_lowercase);
        let mut needle = needle.chars().flat_map(char::to_lowercase);
        loop {
            match (needle.next(), haystack.next()) {
                // The pattern is exhausted: everything matched.
                (None, _) => return true,
                // The line ends before the pattern does.
                (Some(_), None) => return false,
                (Some(m), Some(f)) if m == f => continue,
                _ => return false,
            }
        }
    })
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_ignores_case_including_beyond_ascii() {
        assert!(contains_ignore_case("Connection refused", "REFUSED"));
        // Beyond ASCII too: a log message is not always plain English.
        assert!(contains_ignore_case("CAFÉ queue drained", "café"));
        assert!(contains_ignore_case("app_login", "_LOG"));
        assert!(!contains_ignore_case("app_login", "logout"));
        // An empty pattern filters nothing, and nothing overruns the end.
        assert!(contains_ignore_case("short", ""));
        assert!(!contains_ignore_case("ab", "abc"));
    }
}
