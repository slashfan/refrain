//! Aggregation: this is where a stream of lines becomes useful figures.
//!
//! Every structure in this module is designed for a **fixed memory bound**: 40
//! GB of logs can be swallowed without the footprint moving. The quantiles rest
//! on a bounded-error histogram, the time axis on a ring buffer, and the
//! grouping tables have a ceiling.

use crate::cli::{Cli, DurationUnit};
use crate::parser::{Level, LogEntry};
use chrono::{DateTime, FixedOffset, Local, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Ceilings: beyond them, we stop adding new keys (the ones already known keep
/// being counted). Without this, an identifier slipping into a route would blow
/// the memory up.
const MAX_ROUTES: usize = 4096;
const MAX_ERRORS: usize = 4096;
const MAX_CHANNELS: usize = 512;
const MAX_OPEN_REQUESTS: usize = 20_000;
/// SQL query shapes whose text is kept.
const MAX_SQL_SHAPES: usize = 2048;
/// Distinct N+1 patterns followed (endpoint × SQL query pairs).
const MAX_NPLUS1: usize = 1024;
/// Distinct SQL shapes followed within one HTTP request: beyond this, the total
/// keeps being counted without memorising new shapes.
const MAX_SHAPES_PER_REQUEST: usize = 256;

/// What we have stopped **detailing** the keys of, for lack of room under a
/// ceiling.
///
/// The counters, themselves, carry on: a ceiling that is reached never stops
/// counting. But a table that has silently become incomplete is worse than a
/// missing one — past 4096 routes, nothing allowed you to suspect it, not on
/// screen, not in the JSON, not for a `--fail-if` threshold.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capped {
    pub routes: bool,
    pub errors: bool,
    pub channels: bool,
    pub sql_shapes: bool,
    pub nplus1: bool,
    pub open_requests: bool,
}

impl Capped {
    /// The saturated tables, under the name the user knows them by.
    pub fn names(self) -> Vec<&'static str> {
        [
            (self.routes, "routes"),
            (self.errors, "errors"),
            (self.channels, "channels"),
            (self.sql_shapes, "sql shapes"),
            (self.nplus1, "n+1 patterns"),
            (self.open_requests, "open requests"),
        ]
        .into_iter()
        .filter_map(|(atteint, distinct_name)| atteint.then_some(distinct_name))
        .collect()
    }

    pub fn any(self) -> bool {
        self != Self::default()
    }
}

/// Field names where a duration is looked for, in order of preference.
const DURATION_KEYS: [&str; 9] = [
    "duration_ms",
    "duration",
    "elapsed_ms",
    "elapsed",
    "response_time",
    "execution_time",
    "exec_time",
    "runtime",
    "request_time",
];

/// Field names identifying a request, for correlation.
const CORRELATION_KEYS: [&str; 5] = ["token", "uid", "request_id", "x-request-id", "trace_id"];

// ---------------------------------------------------------------------------
// The time axis
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
pub struct Bucket {
    pub total: u64,
    pub errors: u64,
}

/// A ring buffer of one bucket per second: that is what gives the sparklines
/// and lets peaks be spotted.
///
/// The principle: `head` designates the bucket of the most recent second. When
/// a more recent entry arrives, `head` advances, zeroing the buckets crossed on
/// the way. Nothing is ever allocated after construction.
pub struct Timeline {
    buckets: Vec<Bucket>,
    head: usize,
    head_epoch: i64,
    started: bool,
    /// The busiest second seen since the start, and its epoch.
    ///
    /// It is kept on the fly because the ring, itself, forgets: a post-mortem
    /// file covers hours, the ring ten minutes. Sweeping the buckets to find
    /// the peak therefore did not give the file's peak, but that of its last
    /// ten minutes — a peak of 200 lines/s an hour earlier was announced as 1.
    peak: (u64, i64),
}

impl Timeline {
    pub fn new(seconds: usize) -> Self {
        Self {
            buckets: vec![Bucket::default(); seconds],
            head: 0,
            head_epoch: 0,
            started: false,
            peak: (0, 0),
        }
    }

    pub fn record(&mut self, epoch: i64, is_error: bool) {
        let len = self.buckets.len();
        if !self.started {
            self.started = true;
            self.head_epoch = epoch;
        }

        if epoch > self.head_epoch {
            // We advance by as many seconds as needed, cleaning on the way.
            // `min(len)` avoids a million-turn loop when two files dated months
            // apart follow each other.
            let advance = (epoch - self.head_epoch).min(len as i64) as usize;
            for _ in 0..advance {
                self.head = (self.head + 1) % len;
                self.buckets[self.head] = Bucket::default();
            }
            self.head_epoch = epoch;
        }

        let back = self.head_epoch - epoch;
        if back < 0 || back >= len as i64 {
            return; // too old for the window observed
        }
        let index = (self.head + len - back as usize) % len;
        let bucket = &mut self.buckets[index];
        bucket.total += 1;
        if is_error {
            bucket.errors += 1;
        }
        // One `max` per line, where sweeping the ring cost 600 comparisons on
        // every read of the peak.
        if bucket.total > self.peak.0 {
            self.peak = (bucket.total, epoch);
        }
    }

    /// The last `n` seconds, oldest to most recent.
    pub fn series(&self, n: usize, pick: impl Fn(&Bucket) -> u64) -> Vec<u64> {
        let len = self.buckets.len();
        let n = n.min(len);
        (0..n)
            .map(|i| {
                let back = n - 1 - i;
                pick(&self.buckets[(self.head + len - back) % len])
            })
            .collect()
    }

    /// The busiest second of **everything** that was read: (lines/s, epoch
    /// second). In the dashboard, "everything" starts over at `r`, which
    /// rebuilds the aggregate.
    pub fn peak(&self) -> (u64, i64) {
        self.peak
    }

    /// Average throughput over the last `secs` seconds.
    pub fn rate(&self, secs: usize) -> f64 {
        let sum: u64 = self.series(secs, |b| b.total).iter().sum();
        sum as f64 / secs.max(1) as f64
    }
}

// ---------------------------------------------------------------------------
// Per-key counters
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct ChannelStat {
    pub count: u64,
    pub errors: u64,
}

#[derive(Clone)]
pub struct ErrorStat {
    pub count: u64,
    pub level: Level,
    pub channel: String,
    pub exception: Option<String>,
    pub message: String,
    pub context: Option<String>,
    pub first_seen: Option<DateTime<FixedOffset>>,
    pub last_seen: Option<DateTime<FixedOffset>>,
    pub endpoint: Option<String>,
}

/// Duration histogram with **bounded relative error**.
///
/// Every octave — a factor of two — is cut into 32 slices. The width of a slice
/// is therefore proportional to the value: the error stays under ±1.6 % at
/// every scale, at 1 ms as at 10 s, where fixed-width slices would be
/// ridiculous on one end and coarse on the other.
///
/// The worst case is the bottom of an octave, where a slice weighs the most in
/// proportion: half of 1/32, that is 1.56 %. Sixteen slices per octave would
/// have given 3.1 % — measured, not assumed.
///
/// Twenty-one octaves cover 0.06 ms to 131 s in 672 counters, that is 2.6 KB
/// per route — against 4 KB for the sliding sample it replaces, and above all
/// without forgetting what came before the last 1024 requests.
#[derive(Clone)]
struct Histogram {
    buckets: [u32; Histogram::BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; Self::BUCKETS],
        }
    }
}

impl Histogram {
    /// Slices per octave, a power of two: this is a split of the mantissa, not
    /// a division.
    const SUB: usize = 32;
    /// The smallest duration told apart: 2^-4 ms, that is 62 µs.
    const MIN_EXP: i32 = -4;
    /// Up to 2^17 ms, that is 131 s. Beyond, everything falls into the last
    /// slice — `max_ms`, itself, stays tracked exactly.
    const OCTAVES: usize = 21;
    const BUCKETS: usize = Self::OCTAVES * Self::SUB;

    /// The index is read straight from the bits of the float: the exponent
    /// gives the octave, the first five mantissa bits the slice. Two shifts and
    /// a multiplication — no `log2` in the hot path.
    fn index(ms: f32) -> usize {
        let bits = ms.to_bits();
        let exponent = ((bits >> 23) & 0xff) as i32 - 127;
        if !ms.is_finite() || exponent < Self::MIN_EXP {
            return 0;
        }
        let sub = ((bits >> 18) & 0x1f) as usize;
        let index = (exponent - Self::MIN_EXP) as usize * Self::SUB + sub;
        index.min(Self::BUCKETS - 1)
    }

    /// The representative value of a slice: its middle.
    fn value(index: usize) -> f32 {
        let exponent = (index / Self::SUB) as i32 + Self::MIN_EXP;
        let sub = (index % Self::SUB) as f32;
        let fraction = 1.0 + (sub + 0.5) / Self::SUB as f32;
        fraction * 2f32.powi(exponent)
    }

    fn record(&mut self, ms: f32) {
        let index = Self::index(ms);
        self.buckets[index] = self.buckets[index].saturating_add(1);
    }

    /// The three quantiles in a single pass. `total` is the exact number of
    /// durations recorded, kept apart: it is what gives the ranks.
    fn quantiles(&self, total: u64) -> Quantiles {
        if total == 0 {
            return Quantiles::default();
        }
        // The quantile's rank, as on a sorted array: that was the sample's
        // definition, and changing the storage does not change it.
        let rank = |p: f64| (((total - 1) as f64) * p).round() as u64;
        let (r50, r95, r99) = (rank(0.50), rank(0.95), rank(0.99));

        let mut out = Quantiles::default();
        let mut cumulative = 0u64;
        let mut done = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            cumulative += u64::from(*count);
            let value = Self::value(index);
            if done == 0 && cumulative > r50 {
                out.p50 = value;
                done = 1;
            }
            if done == 1 && cumulative > r95 {
                out.p95 = value;
                done = 2;
            }
            if done == 2 && cumulative > r99 {
                out.p99 = value;
                return out;
            }
        }
        out
    }
}

/// Statistics for one endpoint. The durations live in a bounded-error
/// histogram: the quantiles therefore cover **everything** that was read, for a
/// fixed footprint smaller than a sample.
#[derive(Default, Clone)]
pub struct RouteStat {
    pub requests: u64,
    pub errors: u64,
    pub timed: u64,
    pub sum_ms: f64,
    pub max_ms: f32,
    /// HTTP responses seen for this endpoint — the lines carrying a status.
    /// Denominator of the per-class rates: it is the population the information
    /// exists for, and not the number of requests.
    pub responses: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    /// HTTP requests closed for this endpoint: denominator of the SQL average.
    pub closed_requests: u64,
    pub queries_total: u64,
    pub queries_max: u32,
    /// Allocated on the first duration only: a route nothing is measured on —
    /// and there are some — does not pay for its 672 counters.
    histogram: Option<Box<Histogram>>,
}

impl RouteStat {
    fn add_duration(&mut self, ms: f64) {
        self.timed += 1;
        self.sum_ms += ms;
        let ms = ms as f32;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        self.histogram.get_or_insert_with(Box::default).record(ms);
    }

    /// Quantiles of the durations observed, over the whole window read.
    pub fn quantiles(&self) -> Quantiles {
        match &self.histogram {
            Some(histogram) => histogram.quantiles(self.timed),
            None => Quantiles::default(),
        }
    }

    /// Counts the SQL queries of an HTTP request that has just closed.
    /// Requests with no SQL count too: otherwise the average would be inflated.
    fn add_queries(&mut self, count: u32) {
        self.closed_requests += 1;
        self.queries_total += u64::from(count);
        self.queries_max = self.queries_max.max(count);
    }

    /// SQL queries per HTTP request, on average.
    pub fn avg_queries(&self) -> f32 {
        if self.closed_requests == 0 {
            0.0
        } else {
            self.queries_total as f32 / self.closed_requests as f32
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.timed == 0 {
            0.0
        } else {
            (self.sum_ms / self.timed as f64) as f32
        }
    }

    pub fn error_rate(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f32 / self.requests as f32
        }
    }

    /// Share of responses in 5xx. `None` if none was read: inventing a zero
    /// would make a threshold look respected.
    pub fn rate_5xx(&self) -> Option<f64> {
        match self.responses {
            0 => None,
            responses => Some(self.status_5xx as f64 / responses as f64),
        }
    }

    fn record_status(&mut self, code: u16) {
        self.responses += 1;
        match code / 100 {
            4 => self.status_4xx += 1,
            5 => self.status_5xx += 1,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Quantiles {
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
}

// ---------------------------------------------------------------------------
// Where a request's duration comes from
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum DurationSource {
    /// No usable duration found so far.
    Unknown,
    /// A field of `context`/`extra` carries the duration directly.
    Field { key: String, unit: DurationUnit },
    /// Deduced by correlating the lines of one request through an identifier.
    Correlated { key: String },
}

impl DurationSource {
    pub fn label(&self) -> String {
        match self {
            Self::Unknown => "none".into(),
            Self::Field { key, .. } => format!("field '{key}'"),
            Self::Correlated { key } => format!("correlation on '{key}'"),
        }
    }
}

/// Follows the "open" requests to deduce a duration from them.
///
/// Without a duration field we can still measure: every line of one request
/// shares an identifier (Symfony's `token`, or the `uid` from Monolog's
/// `UidProcessor`). The duration is then the gap between the first and the last
/// line carrying that identifier.
pub struct RequestTracker {
    pub key: Option<String>,
    pub enabled: bool,
    /// The open-request ceiling has been reached: some requests could not be
    /// followed, and their duration will be missing. Picked up by `Stats` in
    /// [`Capped`].
    pub saturated: bool,
    timeout_ms: f64,
    open: HashMap<String, OpenRequest>,
}

struct OpenRequest {
    first_ms: i64,
    last_ms: i64,
    endpoint: Option<String>,
    /// SQL query fingerprint → number of executions within this HTTP request.
    queries: HashMap<u64, u32>,
    /// Total, including the shapes not memorised for lack of room.
    query_count: u32,
}

pub struct FinishedRequest {
    pub endpoint: String,
    pub ms: f64,
    pub queries: Vec<(u64, u32)>,
    pub query_count: u32,
}

/// Where a source stands, for the sweep of correlated requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clock {
    /// Nothing delivered yet: it may hold lines of any date.
    Unknown,
    /// The date of the last line delivered — where the reader stands in its file.
    At(i64),
    /// Caught up with the end of its file: on wall-clock time.
    Live,
    /// Closed for good: holds nothing back.
    Done,
}

/// An entry as the stream keeps it: the parsed line, and the endpoint the
/// aggregation managed to attach it to.
///
/// The line alone is not enough: a Doctrine SQL query, an uncaught exception
/// name no route. It is the token shared with the "Matched route" line that
/// ties them together, and that connection is only known here, at ingestion —
/// impossible to redo at render time. So it is kept with the line, which is
/// what allows following an endpoint all the way into the stream.
pub struct StreamEntry {
    pub entry: LogEntry,
    pub endpoint: Option<String>,
}

/// An N+1 pattern: the same SQL query repeated within a single HTTP request.
#[derive(Clone)]
pub struct NPlusOne {
    pub endpoint: String,
    pub sql: String,
    /// Number of HTTP requests where the pattern was observed.
    pub requests: u64,
    /// Worst repetition seen on a single HTTP request.
    pub max_count: u32,
    total_count: u64,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl NPlusOne {
    /// Repetitions per HTTP request, on average.
    pub fn avg_count(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.total_count as f32 / self.requests as f32
        }
    }
}

impl RequestTracker {
    fn new(key: Option<String>, enabled: bool, timeout_secs: f64) -> Self {
        Self {
            key,
            enabled,
            saturated: false,
            timeout_ms: timeout_secs * 1000.0,
            open: HashMap::new(),
        }
    }

    /// Looks for the request identifier in the entry, locking the key in as
    /// soon as one that works has been found.
    fn token_of<'a>(&mut self, entry: &'a LogEntry) -> Option<&'a str> {
        if !self.enabled {
            return None;
        }
        if let Some(key) = &self.key {
            return entry.lookup(key).and_then(value_as_token);
        }
        for candidate in CORRELATION_KEYS {
            if let Some(token) = entry.lookup(candidate).and_then(value_as_token) {
                self.key = Some(candidate.to_string());
                return Some(token);
            }
        }
        None
    }

    /// Records the line in its request and returns the endpoint known for it —
    /// which is what attributes an error carrying no route context.
    fn observe(
        &mut self,
        entry: &LogEntry,
        endpoint: Option<&str>,
        ms: i64,
        sql: Option<u64>,
    ) -> Option<String> {
        let token = self.token_of(entry)?.to_string();

        if self.open.len() >= MAX_OPEN_REQUESTS && !self.open.contains_key(&token) {
            self.saturated = true;
            return None;
        }
        let open = self.open.entry(token).or_insert_with(|| OpenRequest {
            first_ms: ms,
            last_ms: ms,
            endpoint: None,
            queries: HashMap::new(),
            query_count: 0,
        });
        open.last_ms = open.last_ms.max(ms);
        open.first_ms = open.first_ms.min(ms);
        if let Some(endpoint) = endpoint {
            open.endpoint = Some(endpoint.to_string());
        }
        if let Some(fingerprint) = sql {
            open.query_count = open.query_count.saturating_add(1);
            let connue = open.queries.contains_key(&fingerprint);
            if connue || open.queries.len() < MAX_SHAPES_PER_REQUEST {
                *open.queries.entry(fingerprint).or_insert(0) += 1;
            }
        }
        open.endpoint.clone()
    }

    /// Closes the requests with no new line since `timeout_ms`.
    fn sweep(&mut self, now_ms: i64) -> Vec<FinishedRequest> {
        let mut done = Vec::new();
        // `retain` walks the table once and removes on the way: far more
        // efficient than collecting the keys then removing them one by one.
        self.open.retain(|_, open| {
            if ((now_ms - open.last_ms) as f64) < self.timeout_ms {
                return true;
            }
            if let Some(endpoint) = &open.endpoint {
                done.push(FinishedRequest {
                    endpoint: endpoint.clone(),
                    ms: (open.last_ms - open.first_ms) as f64,
                    // `take` recovers the table without copying it: the request
                    // is destroyed right after, anyway.
                    queries: std::mem::take(&mut open.queries).into_iter().collect(),
                    query_count: open.query_count,
                });
            }
            false
        });
        done
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

fn value_as_token(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// The complete aggregate
// ---------------------------------------------------------------------------

pub struct Stats {
    pub total: u64,
    /// HTTP requests seen, all endpoints together — including those the
    /// `MAX_ROUTES` ceiling kept from being detailed, and those whose name we
    /// could not read: this is a denominator, it must count everything.
    pub requests: u64,
    pub skipped: u64,
    /// Lines dropped because they fall outside the `--since`/`--until` window.
    /// There is nothing unreadable about them: counting them with `skipped`
    /// would hide a real format problem behind a filter doing its job.
    pub out_of_window: u64,
    /// The window bounds, resolved at start-up.
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    pub by_level: [u64; 8],
    /// HTTP responses by class: 1xx at 0, … 5xx at 4. Monolog writes no status
    /// of its own; when the application logs one, it is the only way to tell a
    /// 500 from a noisy 404 — the logging level, itself, only says what the
    /// developer chose to write.
    pub by_status: [u64; 5],
    pub channels: HashMap<String, ChannelStat>,
    pub errors: HashMap<String, ErrorStat>,
    pub routes: HashMap<String, RouteStat>,
    /// N+1 patterns, indexed by (endpoint, SQL query fingerprint).
    pub nplus1: HashMap<(String, u64), NPlusOne>,
    /// Fingerprint → SQL text dictionary: the text is stored once only, and
    /// not inside each of the open requests.
    sql_texts: HashMap<u64, String>,
    nplus1_threshold: u32,
    pub timeline: Timeline,
    pub recent: VecDeque<StreamEntry>,
    pub tracker: RequestTracker,
    pub duration: DurationSource,
    /// The tables that have stopped detailing. See [`Capped`].
    pub capped: Capped,
    pub first_ts: Option<DateTime<FixedOffset>>,
    pub last_ts: Option<DateTime<FixedOffset>>,
    forced_unit: DurationUnit,
    forced_key: Option<String>,
    scrollback: usize,
    /// Where each source stands. The sweep of correlated requests paces itself
    /// on the furthest behind of them — otherwise a file read faster than the
    /// others would close requests whose lines are still waiting to be read.
    clocks: Vec<Clock>,
    last_sweep_ms: i64,
    saw_matched_route: bool,
    wall_guard_ms: i64,
}

impl Stats {
    pub fn new(cli: &Cli) -> Self {
        let launched_ms = Utc::now().timestamp_millis();
        Self {
            total: 0,
            requests: 0,
            skipped: 0,
            out_of_window: 0,
            // Resolved once and for all: "--since 15m" designates a fixed
            // instant, not a window sliding as the analysis goes.
            since_ms: cli.since.map(|b| b.epoch_ms(launched_ms)),
            until_ms: cli.until.map(|b| b.epoch_ms(launched_ms)),
            by_level: [0; 8],
            by_status: [0; 5],
            channels: HashMap::new(),
            errors: HashMap::new(),
            routes: HashMap::new(),
            nplus1: HashMap::new(),
            sql_texts: HashMap::new(),
            nplus1_threshold: cli.nplus1,
            timeline: Timeline::new(600),
            recent: VecDeque::with_capacity(cli.scrollback.min(1024)),
            tracker: RequestTracker::new(
                cli.correlate_key.clone(),
                !cli.no_correlate,
                cli.correlate_timeout,
            ),
            duration: DurationSource::Unknown,
            capped: Capped::default(),
            first_ts: None,
            last_ts: None,
            forced_unit: cli.duration_unit,
            forced_key: cli.duration_key.clone(),
            scrollback: cli.scrollback,
            clocks: vec![Clock::Unknown; cli.files.len().max(1)],
            last_sweep_ms: i64::MIN,
            saw_matched_route: false,
            wall_guard_ms: i64::MIN,
        }
    }

    pub fn reset(&mut self, cli: &Cli) {
        *self = Self::new(cli);
    }

    pub fn ingest(&mut self, source: usize, entry: LogEntry) {
        // Reference clock: the logs' when there is one (that is what replays an
        // old file with its peaks in the right place), otherwise ours.
        let now_ms = match entry.ts {
            Some(ts) => ts.timestamp_millis(),
            None => Utc::now().timestamp_millis(),
        };

        // The window is judged before any counting. A line outside it must
        // weigh nowhere: not in the totals, not on the time axis, not in the
        // quantiles — otherwise "--since 15m" would give a p95 computed over
        // the whole day.
        if self.outside_window(now_ms) {
            self.out_of_window += 1;
            return;
        }

        if let Some(ts) = entry.ts {
            self.last_ts = Some(ts);
            self.first_ts.get_or_insert(ts);
        }
        self.total += 1;
        self.by_level[entry.level.index()] += 1;
        let is_error = entry.level.is_error();
        self.set_clock(source, now_ms);
        // The guard only holds for the time axis: durations, themselves, must
        // stay computed on the real dates of the lines.
        let bucket_ms = self.clamp_future(now_ms);
        self.timeline.record(bucket_ms.div_euclid(1000), is_error);

        // -- channels ------------------------------------------------------
        if self.channels.len() < MAX_CHANNELS || self.channels.contains_key(&entry.channel) {
            let channel = self.channels.entry(entry.channel.clone()).or_default();
            channel.count += 1;
            if is_error {
                channel.errors += 1;
            }
        } else {
            self.capped.channels = true;
        }

        // -- duration ------------------------------------------------------
        let field_ms = self.duration_of(&entry);
        // Doctrine logs every query in `context.sql`. We reduce it to a 64-bit
        // fingerprint, memorising its text only once.
        let sql = entry
            .lookup("sql")
            .and_then(Value::as_str)
            .map(|sql| self.intern_sql(sql));
        let own_endpoint = entry.endpoint();
        let known_endpoint = self
            .tracker
            .observe(&entry, own_endpoint.as_deref(), now_ms, sql);
        let endpoint = own_endpoint.or(known_endpoint);

        // A "request" = a "Matched route" line: Symfony writes exactly one per
        // HTTP request, it is the most reliable marker. If the stream contains
        // none, we fall back on the lines carrying a duration.
        let matched = is_matched_route(&entry);
        self.saw_matched_route |= matched;
        let counts_as_request = matched || (!self.saw_matched_route && field_ms.is_some());
        // Outside the block that follows, and therefore outside the route
        // ceiling: the request total serves as a denominator, it must not stop
        // growing when the table stops detailing.
        if counts_as_request {
            self.requests += 1;
        }

        let status = entry.status();
        if let Some(code) = status {
            self.by_status[(code / 100 - 1) as usize] += 1;
        }

        if let Some(name) = &endpoint
            && (counts_as_request || is_error || field_ms.is_some() || status.is_some())
        {
            if self.routes.len() < MAX_ROUTES || self.routes.contains_key(name) {
                let route = self.routes.entry(name.clone()).or_default();
                if counts_as_request {
                    route.requests += 1;
                }
                if is_error {
                    route.errors += 1;
                }
                if let Some(ms) = field_ms {
                    route.add_duration(ms);
                }
                if let Some(code) = status {
                    route.record_status(code);
                }
            } else {
                self.capped.routes = true;
            }
        }

        // -- errors --------------------------------------------------------
        if is_error {
            self.record_error(&entry, endpoint.clone());
        }

        // -- stream --------------------------------------------------------
        if self.recent.len() >= self.scrollback {
            self.recent.pop_front();
        }
        self.recent.push_back(StreamEntry { entry, endpoint });

        // -- closing correlated requests ----------------------------------
        // Once a second is enough: `sweep` walks the whole table.
        // `saturating_sub`: on the very first call `last_sweep_ms` is i64::MIN,
        // and an ordinary subtraction would overflow.
        // The tracker holds its own ceiling; we pick it up here so that the six
        // tables are read in one place.
        self.capped.open_requests |= self.tracker.saturated;

        if let Some(watermark) = self.watermark(now_ms)
            && watermark.saturating_sub(self.last_sweep_ms) > 1000
        {
            self.last_sweep_ms = watermark;
            self.close_finished(watermark);
        }
    }

    /// The synchronisation point between sources: the date up to which *all*
    /// of them have delivered their lines. `None` while a source has delivered
    /// nothing at all: it may hold lines of any date, nothing can be closed.
    ///
    /// Each file is read by its own thread, as fast as it can. Two files
    /// covering the same period therefore have no reason to progress through
    /// it at the same speed: `prod.log` may have reached noon while
    /// `doctrine.log` is still at ten. Sweeping on the fastest one's clock
    /// would close the slowest one's requests before its lines had even been
    /// read. So we pace on the furthest-behind source — and a source that has
    /// caught up with its file stands at `now`, whatever `now` is for the
    /// caller: the date of the line being ingested, or the wall clock when
    /// nothing arrives.
    fn watermark(&self, now: i64) -> Option<i64> {
        let mut earliest = None;
        for clock in &self.clocks {
            let ms = match clock {
                Clock::Unknown => return None,
                Clock::At(ms) => *ms,
                Clock::Live => now,
                Clock::Done => continue,
            };
            earliest = Some(earliest.map_or(ms, |e: i64| e.min(ms)));
        }
        // Every source has run dry: nothing left to wait for.
        Some(earliest.unwrap_or(now))
    }

    /// Does this date fall outside the window asked for?
    fn outside_window(&self, ms: i64) -> bool {
        self.since_ms.is_some_and(|depuis| ms < depuis)
            || self.until_ms.is_some_and(|jusqu| ms > jusqu)
    }

    /// Is a window in force? Used so that "out of window" entries are only
    /// mentioned when there is one.
    pub fn windowed(&self) -> bool {
        self.since_ms.is_some() || self.until_ms.is_some()
    }

    /// The clock follows the last line delivered, never overstating it: that
    /// is where the reader stands, and a single source thus recovers exactly
    /// the previous behaviour — pacing on the maximum seen would close earlier.
    fn set_clock(&mut self, source: usize, ms: i64) {
        if let Some(clock) = self.clocks.get_mut(source) {
            *clock = Clock::At(ms);
        }
    }

    /// A source has caught up with the end of its file: its next lines will
    /// arrive live, so it is on wall-clock time until one does.
    pub fn source_caught_up(&mut self, source: usize) {
        if let Some(clock) = self.clocks.get_mut(source) {
            *clock = Clock::Live;
        }
    }

    /// A source is closed for good: it no longer holds back the sweep.
    pub fn source_done(&mut self, source: usize) {
        if let Some(clock) = self.clocks.get_mut(source) {
            *clock = Clock::Done;
        }
    }

    /// Stops a line dated in the future from propelling the time axis.
    ///
    /// A single line ahead — clock drift, a log copied from another machine, a
    /// very long request logged when it opened — would otherwise be enough to
    /// empty the whole ring buffer and display a throughput of zero. The clock
    /// is only read when a date exceeds the last guard, that is about once a
    /// second when following live.
    fn clamp_future(&mut self, ts_ms: i64) -> i64 {
        if ts_ms <= self.wall_guard_ms {
            return ts_ms;
        }
        self.wall_guard_ms = Utc::now().timestamp_millis() + 1_000;
        ts_ms.min(self.wall_guard_ms)
    }

    fn record_error(&mut self, entry: &LogEntry, endpoint: Option<String>) {
        let signature = entry.signature();
        if self.errors.len() >= MAX_ERRORS && !self.errors.contains_key(&signature) {
            self.capped.errors = true;
            return;
        }
        let stat = self.errors.entry(signature).or_insert_with(|| ErrorStat {
            count: 0,
            level: entry.level,
            channel: entry.channel.clone(),
            exception: entry.exception_class().map(|c| c.to_string()),
            message: String::new(),
            context: None,
            first_seen: entry.ts,
            last_seen: entry.ts,
            endpoint: None,
        });

        stat.count += 1;
        stat.last_seen = entry.ts.or(stat.last_seen);
        // The most severe level seen for this signature is always kept.
        stat.level = stat.level.max(entry.level);
        stat.message = entry.message.clone();
        stat.context = entry
            .context
            .as_ref()
            .and_then(|c| serde_json::to_string_pretty(c).ok());
        if let Some(endpoint) = endpoint {
            stat.endpoint = Some(match entry.method() {
                Some(method) => format!("{method} {endpoint}"),
                None => endpoint,
            });
        }
    }

    fn close_finished(&mut self, now_ms: i64) -> usize {
        // When a duration field exists, it rules. We keep following requests
        // (that is what attaches an error to its endpoint), but we do not
        // inject a second measurement for the same request.
        let field_mode = matches!(self.duration, DurationSource::Field { .. });

        let threshold = self.nplus1_threshold;
        let seen_at = self.last_ts;
        let mut closed = 0;

        for finished in self.tracker.sweep(now_ms) {
            closed += 1;
            // An N+1 is the same SQL query repeated within a single HTTP
            // request. Since Doctrine logs *prepared* statements
            // (`WHERE id = ?`), two executions of one pattern produce exactly
            // the same string: equality is enough, there is no normalisation
            // to write.
            if threshold > 0 {
                for (fingerprint, count) in &finished.queries {
                    if *count >= threshold {
                        self.record_nplus1(&finished.endpoint, *fingerprint, *count, seen_at);
                    }
                }
            }

            let is_new = !self.routes.contains_key(&finished.endpoint);
            if is_new && self.routes.len() >= MAX_ROUTES {
                self.capped.routes = true;
                continue;
            }
            let route = self.routes.entry(finished.endpoint).or_default();
            route.add_queries(finished.query_count);
            if !field_mode {
                route.add_duration(finished.ms);
            }
        }
        // Correlation is only announced as the source once it really produces
        // measurements: displaying an empty promise would be misleading.
        if matches!(self.duration, DurationSource::Unknown)
            && let Some(key) = &self.tracker.key
            && self.routes.values().any(|r| r.timed > 0)
        {
            self.duration = DurationSource::Correlated { key: key.clone() };
        }
        closed
    }

    fn record_nplus1(
        &mut self,
        endpoint: &str,
        fingerprint: u64,
        count: u32,
        seen_at: Option<DateTime<FixedOffset>>,
    ) {
        let key = (endpoint.to_string(), fingerprint);
        if self.nplus1.len() >= MAX_NPLUS1 && !self.nplus1.contains_key(&key) {
            self.capped.nplus1 = true;
            return;
        }
        let sql = self
            .sql_texts
            .get(&fingerprint)
            .cloned()
            .unwrap_or_default();
        let pattern = self.nplus1.entry(key).or_insert_with(|| NPlusOne {
            endpoint: endpoint.to_string(),
            sql,
            requests: 0,
            max_count: 0,
            total_count: 0,
            last_seen: None,
        });
        pattern.requests += 1;
        pattern.max_count = pattern.max_count.max(count);
        pattern.total_count += u64::from(count);
        pattern.last_seen = seen_at.or(pattern.last_seen);
    }

    /// 64-bit fingerprint of an SQL query, whose text is memorised on the way.
    fn intern_sql(&mut self, sql: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        sql.hash(&mut hasher);
        let fingerprint = hasher.finish();

        if self.sql_texts.len() >= MAX_SQL_SHAPES {
            // A fingerprint already known keeps its text: only a new shape is
            // refused, and it alone is what we report.
            self.capped.sql_shapes |= !self.sql_texts.contains_key(&fingerprint);
        } else {
            self.sql_texts.entry(fingerprint).or_insert_with(|| {
                // Whitespace normalised: logged SQL is sometimes indented over
                // several lines, which makes it unreadable in a table.
                let mut text = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                crate::parser::truncate_chars(&mut text, 400);
                text
            });
        }
        fingerprint
    }

    /// Closes pending requests when nothing arrives any more.
    ///
    /// When following live, the last request stays open as long as no new line
    /// advances the log clock. We then fall back on the wall clock — but only
    /// for the sources that have caught up with their file. One still behind,
    /// its reader stalled on a slow disk in the middle of yesterday's log,
    /// holds the sweep at its own date: closing on the wall clock would cut
    /// every request it has open in two, and an N+1 split in two halves never
    /// crosses the threshold again.
    /// Returns the number of requests actually closed.
    pub fn sweep_idle(&mut self) -> usize {
        match self.watermark(Utc::now().timestamp_millis()) {
            Some(watermark) => self.close_finished(watermark),
            None => 0,
        }
    }

    /// Empties the requests still open: called at end of file, otherwise the
    /// last handful of requests would never be counted.
    pub fn finalize(&mut self) {
        let now_ms = self
            .last_ts
            .map(|t| t.timestamp_millis())
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        // A sweep far into the future closes everything.
        self.close_finished(now_ms.saturating_add(1_000_000_000));
    }

    /// Extracts a duration in milliseconds from the entry, remembering which
    /// field was used the first time one is found.
    fn duration_of(&mut self, entry: &LogEntry) -> Option<f64> {
        if let Some(key) = &self.forced_key {
            let value = entry.lookup(key)?;
            return to_millis(value, key, self.forced_unit);
        }
        if let DurationSource::Field { key, unit } = &self.duration {
            let value = entry.lookup(key)?;
            return to_millis(value, key, *unit);
        }
        if entry.context.is_none() && entry.extra.is_none() {
            return None;
        }
        for candidate in DURATION_KEYS {
            if let Some(value) = entry.lookup(candidate)
                && let Some(ms) = to_millis(value, candidate, self.forced_unit)
            {
                self.duration = DurationSource::Field {
                    key: candidate.to_string(),
                    unit: self.forced_unit,
                };
                return Some(ms);
            }
        }
        None
    }

    /// Number of distinct SQL query shapes met.
    pub fn sql_shapes(&self) -> usize {
        self.sql_texts.len()
    }

    /// Error lines divided by HTTP requests — the definition already used per
    /// endpoint, and the only one that does not move with the number of files
    /// handed over to read.
    ///
    /// `None` when no request was seen: returning 0 would make the threshold
    /// look respected when we have nothing to say about it, exactly like a
    /// quantile on an endpoint that never appeared.
    pub fn request_error_rate(&self) -> Option<f64> {
        match self.requests {
            0 => None,
            requests => Some(self.errors_total() as f64 / requests as f64),
        }
    }

    /// HTTP responses read, all classes together.
    pub fn responses(&self) -> u64 {
        self.by_status.iter().sum()
    }

    /// Share of responses in 5xx. `None` when no status was read at all: the
    /// question then has no answer, and zero would pass itself off as one.
    pub fn rate_5xx(&self) -> Option<f64> {
        match self.responses() {
            0 => None,
            responses => Some(self.by_status[4] as f64 / responses as f64),
        }
    }

    pub fn errors_total(&self) -> u64 {
        Level::ALL
            .iter()
            .filter(|l| l.is_error())
            .map(|l| self.by_level[l.index()])
            .sum()
    }

    /// Time span covered by the analysed logs, in seconds.
    pub fn span_secs(&self) -> f64 {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) => (b - a).num_milliseconds() as f64 / 1000.0,
            _ => 0.0,
        }
    }
}

/// The marker Symfony writes exactly once per HTTP request.
fn is_matched_route(entry: &LogEntry) -> bool {
    entry.channel == "request" && entry.message.starts_with("Matched route")
}

/// Converts the value of a duration field into milliseconds.
///
/// Accepts numbers (`123.5`) as well as strings (`"123ms"`, `"1.5s"`).
fn to_millis(value: &Value, key: &str, forced: DurationUnit) -> Option<f64> {
    let (raw, unit_from_value) = match value {
        Value::Number(n) => (n.as_f64()?, None),
        Value::String(s) => parse_number_with_unit(s)?,
        _ => return None,
    };
    if raw < 0.0 || !raw.is_finite() {
        return None;
    }

    let unit = match (forced, unit_from_value) {
        (DurationUnit::Auto, Some(unit)) => unit,
        (DurationUnit::Auto, None) => infer_unit(key, raw),
        (forced, _) => forced,
    };

    Some(match unit {
        DurationUnit::S => raw * 1000.0,
        DurationUnit::Us => raw / 1000.0,
        _ => raw,
    })
}

/// `"1.5s"` → `(1.5, Some(S))`, `"120"` → `(120.0, None)`
fn parse_number_with_unit(s: &str) -> Option<(f64, Option<DurationUnit>)> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let value: f64 = s[..split].parse().ok()?;
    let unit = match s[split..].trim().to_ascii_lowercase().as_str() {
        "" => None,
        "ms" | "msec" => Some(DurationUnit::Ms),
        "s" | "sec" | "secs" => Some(DurationUnit::S),
        "us" | "µs" | "μs" => Some(DurationUnit::Us),
        _ => return None,
    };
    Some((value, unit))
}

/// Guesses the unit: first from the key name's suffix, which is reliable; then
/// from the order of magnitude, since PHP traditionally measures in floating
/// seconds (`microtime(true)` gives 0.0123 for 12 ms).
fn infer_unit(key: &str, value: f64) -> DurationUnit {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_ms") || key.ends_with("ms") {
        DurationUnit::Ms
    } else if key.ends_with("_us") || key.ends_with("micro") {
        DurationUnit::Us
    } else if key.ends_with("_s")
        || key.ends_with("_sec")
        || key.ends_with("seconds")
        // No telling suffix: a float below 30 is a `microtime(true)`, so
        // seconds. An integer, itself, is almost always milliseconds.
        || (value > 0.0 && value < 30.0 && value.fract() != 0.0)
    {
        DurationUnit::S
    } else {
        DurationUnit::Ms
    }
}

/// Formats a duration for display: "12.3 ms", "1.24 s".
pub fn format_ms(ms: f32) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

/// Formats a large number with separators: "1,234,567".
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        // The comma and not a thin space: the interface speaks English, and
        // an English speaker reads "1,234,567".
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn format_time(ts: Option<DateTime<FixedOffset>>) -> String {
    match ts {
        Some(ts) => ts.with_timezone(&Local).format("%H:%M:%S").to_string(),
        None => "--:--:--".into(),
    }
}

/// The text summary of `--summary` mode.
pub fn render_summary(stats: &Stats) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(out, "── refrain ─ summary ───────────────────────────");
    let _ = writeln!(
        out,
        "{} entries analysed ({} skipped), {} errors",
        format_count(stats.total),
        format_count(stats.skipped),
        format_count(stats.errors_total())
    );
    if stats.windowed() {
        let _ = writeln!(
            out,
            "window   : {} lines dropped outside the bounds",
            format_count(stats.out_of_window)
        );
    }
    if stats.span_secs() > 0.0 {
        let _ = writeln!(
            out,
            "period   : {} → {} ({:.0} s)",
            format_time(stats.first_ts),
            format_time(stats.last_ts),
            stats.span_secs()
        );
    }
    if let Some(rate) = stats.request_error_rate() {
        let _ = writeln!(
            out,
            "requests : {} (error rate {:.2} %)",
            format_count(stats.requests),
            rate * 100.0
        );
    }
    if let Some(rate) = stats.rate_5xx() {
        let classes: Vec<String> = [(1, "1xx"), (2, "2xx"), (3, "3xx"), (4, "4xx"), (5, "5xx")]
            .iter()
            .filter(|(classe, _)| stats.by_status[classe - 1] > 0)
            .map(|(classe, distinct_name)| {
                format!(
                    "{distinct_name} {}",
                    format_count(stats.by_status[classe - 1])
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "status   : {} — {:.2} % 5xx",
            classes.join(" · "),
            rate * 100.0
        );
    }
    let (peak, _) = stats.timeline.peak();
    let _ = writeln!(out, "peak     : {} lines/s", format_count(peak));
    let _ = writeln!(out, "durations: {}", stats.duration.label());
    if stats.capped.any() {
        let _ = writeln!(
            out,
            "capped   : {} — new keys no longer detailed, counters keep counting",
            stats.capped.names().join(", ")
        );
    }

    let _ = writeln!(out, "\nLevels");
    for level in Level::ALL.iter().rev() {
        let count = stats.by_level[level.index()];
        if count > 0 {
            let _ = writeln!(out, "  {:<10} {:>10}", level.as_str(), format_count(count));
        }
    }

    // The name breaks ties: a hash map does not enumerate twice in the same
    // order, and two routes tying on p95 — a common thing since the quantiles
    // come out of a histogram — would come out in a different order from one
    // run to the next. A report has to be comparable from one day to the next.
    let mut errors: Vec<_> = stats.errors.iter().collect();
    errors.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    if !errors.is_empty() {
        let _ = writeln!(out, "\nTop errors");
        for (signature, stat) in errors.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:>7} × [{}] {}",
                format_count(stat.count),
                stat.channel,
                signature
            );
        }
    }

    // The quantiles are computed once per route, then sorted: recomputing
    // them inside the comparator would redo it O(n log n) times.
    let mut routes: Vec<_> = stats
        .routes
        .iter()
        .filter(|(_, route)| route.timed > 0)
        .map(|(name, route)| (name, route, route.quantiles()))
        .collect();
    routes.sort_unstable_by(|a, b| b.2.p95.total_cmp(&a.2.p95).then_with(|| a.0.cmp(b.0)));

    if !routes.is_empty() {
        let _ = writeln!(out, "\nSlowest endpoints (p95)");
        for (name, route, quantiles) in routes.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:<40} n={:<6} p50={:<10} p95={:<10} max={}",
                truncate(name, 40),
                route.requests.max(route.timed),
                format_ms(quantiles.p50),
                format_ms(quantiles.p95),
                format_ms(route.max_ms)
            );
        }
    } else if !stats.routes.is_empty() {
        let _ = writeln!(
            out,
            "\nNo measurable durations. See 'Measuring durations' in the README."
        );
    }

    let mut patterns: Vec<&NPlusOne> = stats.nplus1.values().collect();
    patterns.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| (&a.endpoint, &a.sql).cmp(&(&b.endpoint, &b.sql)))
    });
    if !patterns.is_empty() {
        let _ = writeln!(
            out,
            "\nN+1 patterns (the same SQL query repeated within one HTTP request)"
        );
        for pattern in patterns.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:<22} {:>4} × at worst, {:>5.1} × on average over {} requests",
                truncate(&pattern.endpoint, 22),
                pattern.max_count,
                pattern.avg_count(),
                format_count(pattern.requests)
            );
            let _ = writeln!(out, "      {}", truncate(&pattern.sql, 90));
        }
    }
    out
}

/// Snapshot of the counters in JSON, for monitoring.
///
/// The totals are **cumulative** since start-up, the way a Prometheus counter
/// is: it is up to the collector to take the differences from one reading to
/// the next. `throughput` additionally provides sliding-window rates, usable
/// as they are without any state on the collector's side.
pub fn render_json(stats: &Stats, top: usize, pretty: bool) -> String {
    let levels: serde_json::Map<String, Value> = Level::ALL
        .iter()
        .map(|level| {
            (
                level.lower().to_string(),
                json!(stats.by_level[level.index()]),
            )
        })
        .collect();

    let mut channels: Vec<_> = stats.channels.iter().collect();
    channels.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    let channels: Vec<Value> = channels
        .iter()
        .map(|(name, channel)| {
            json!({ "channel": name, "count": channel.count, "errors": channel.errors })
        })
        .collect();

    let mut errors: Vec<_> = stats.errors.iter().collect();
    errors.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    keep_top(&mut errors, top);
    let errors: Vec<Value> = errors
        .iter()
        .map(|(signature, error)| {
            json!({
                "signature": signature,
                "count": error.count,
                "level": error.level.lower(),
                "channel": error.channel,
                "exception": error.exception,
                "endpoint": error.endpoint,
                "first_seen": error.first_seen.map(|ts| ts.to_rfc3339()),
                "last_seen": error.last_seen.map(|ts| ts.to_rfc3339()),
                "message": error.message.lines().next().unwrap_or_default(),
            })
        })
        .collect();

    let mut endpoints: Vec<_> = stats
        .routes
        .iter()
        .map(|(name, route)| (name, route, route.quantiles()))
        .collect();
    endpoints.sort_unstable_by(|a, b| b.2.p95.total_cmp(&a.2.p95).then_with(|| a.0.cmp(b.0)));
    keep_top(&mut endpoints, top);
    let endpoints: Vec<Value> = endpoints
        .iter()
        .map(|(name, route, quantiles)| {
            json!({
                "endpoint": name,
                "requests": route.requests.max(route.timed),
                "errors": route.errors,
                "error_rate": round(f64::from(route.error_rate()), 4),
                "responses": route.responses,
                "status_4xx": route.status_4xx,
                "status_5xx": route.status_5xx,
                "timed": route.timed,
                "p50_ms": round(f64::from(quantiles.p50), 2),
                "p95_ms": round(f64::from(quantiles.p95), 2),
                "p99_ms": round(f64::from(quantiles.p99), 2),
                "max_ms": round(f64::from(route.max_ms), 2),
                "avg_ms": round(f64::from(route.avg_ms()), 2),
                "queries_avg": round(f64::from(route.avg_queries()), 1),
                "queries_max": route.queries_max,
            })
        })
        .collect();

    let mut patterns: Vec<&NPlusOne> = stats.nplus1.values().collect();
    patterns.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| (&a.endpoint, &a.sql).cmp(&(&b.endpoint, &b.sql)))
    });
    keep_top(&mut patterns, top);
    let nplus1: Vec<Value> = patterns
        .iter()
        .map(|pattern| {
            json!({
                "endpoint": pattern.endpoint,
                "sql": pattern.sql,
                "requests_affected": pattern.requests,
                "max_per_request": pattern.max_count,
                "avg_per_request": round(f64::from(pattern.avg_count()), 1),
                "last_seen": pattern.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let (peak, peak_epoch) = stats.timeline.peak();
    let errors_total = stats.errors_total();

    let snapshot = json!({
        "generated_at": Local::now().to_rfc3339(),
        "window": {
            "first_seen": stats.first_ts.map(|ts| ts.to_rfc3339()),
            "last_seen": stats.last_ts.map(|ts| ts.to_rfc3339()),
            "span_seconds": round(stats.span_secs(), 3),
        },
        "totals": {
            "entries": stats.total,
            "skipped": stats.skipped,
            "errors": errors_total,
            // Two denominators, two uses: `error_rate` says what share of the
            // lines are errors — it therefore depends on what you hand over to
            // read — while `request_error_rate` divides the same errors by HTTP
            // requests and does not move when `doctrine.log` is added.
            "error_rate": round(ratio(errors_total, stats.total), 4),
            "requests": stats.requests,
            "request_error_rate": stats.request_error_rate().map(|r| round(r, 4)),
            "out_of_window": stats.out_of_window,
        },
        "levels": levels,
        // The response classes, when the application logs a status: that is
        // the only way to tell a 500 from a noisy 404, which the logging level
        // conflates.
        "status": {
            "responses": stats.responses(),
            "1xx": stats.by_status[0],
            "2xx": stats.by_status[1],
            "3xx": stats.by_status[2],
            "4xx": stats.by_status[3],
            "5xx": stats.by_status[4],
            "rate_5xx": stats.rate_5xx().map(|r| round(r, 4)),
        },
        "throughput": {
            "peak_per_second": peak,
            "peak_at": (peak > 0).then(|| epoch_to_rfc3339(peak_epoch)).flatten(),
            "last_5s_per_second": round(stats.timeline.rate(5), 2),
            "last_60s_per_second": round(stats.timeline.rate(60), 2),
        },
        "duration_source": match &stats.duration {
            DurationSource::Unknown => json!({ "kind": "none" }),
            DurationSource::Field { key, .. } => json!({ "kind": "field", "key": key }),
            DurationSource::Correlated { key } => json!({ "kind": "correlation", "key": key }),
        },
        "open_requests": stats.tracker.open_count(),
        // Empty the rest of the time: what it holds is no longer detailed in
        // full, and the matching lists are therefore partial.
        "capped": stats.capped.names(),
        "sql": {
            "shapes": stats.sql_shapes(),
            "nplus1_threshold": stats.nplus1_threshold,
        },
        "channels": channels,
        "errors": errors,
        "endpoints": endpoints,
        "nplus1": nplus1,
    });

    if pretty {
        serde_json::to_string_pretty(&snapshot).unwrap_or_default()
    } else {
        snapshot.to_string()
    }
}

/// Keeps only the first `top` items. `0` means "keep everything".
fn keep_top<T>(items: &mut Vec<T>, top: usize) {
    if top > 0 {
        items.truncate(top);
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Rounds, so the output is not drowned under fifteen decimals of float noise.
fn round(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn epoch_to_rfc3339(epoch: i64) -> Option<String> {
    DateTime::from_timestamp(epoch, 0).map(|ts| ts.with_timezone(&Local).to_rfc3339())
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_line;
    use clap::Parser;
    use serde_json::json;

    fn stats() -> Stats {
        Stats::new(&Cli::parse_from(["refrain", "prod.log"]))
    }

    /// A distinct name per index, without a single digit.
    ///
    /// An error signature normalises numbers into "#": "Error 1" and "Error 2"
    /// would count as one and the same signature, and the ceiling would never
    /// be reached.
    fn distinct_name(mut index: usize) -> String {
        let mut out = String::new();
        loop {
            out.push((b'a' + (index % 26) as u8) as char);
            index /= 26;
            if index == 0 {
                return out;
            }
        }
    }

    fn ingest_line(stats: &mut Stats, line: &str) {
        stats.ingest(0, parse_line(line).expect("valid line"));
    }

    /// A "Matched route" line, which is enough to create an endpoint.
    fn route_line(name: &str) -> String {
        format!(
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{name}". {{"route":"{name}"}} []"#
        )
    }

    #[test]
    fn the_route_ceiling_stops_the_table_without_stopping_the_counters() {
        let mut stats = stats();
        for i in 0..MAX_ROUTES {
            ingest_line(&mut stats, &route_line(&distinct_name(i)));
        }
        assert_eq!(stats.routes.len(), MAX_ROUTES, "the table is full");

        assert!(!stats.capped.routes, "nothing has been refused yet");

        // One more unknown route: it does not get in.
        ingest_line(&mut stats, &route_line("route_de_trop"));
        assert!(stats.capped.routes, "and the refusal must show");
        assert_eq!(stats.routes.len(), MAX_ROUTES);
        assert!(!stats.routes.contains_key("route_de_trop"));

        // But a route already known keeps being counted: a ceiling that froze
        // the existing counters would turn a memory bound into data loss.
        ingest_line(&mut stats, &route_line(&distinct_name(0)));
        assert_eq!(stats.routes[&distinct_name(0)].requests, 2);
        assert_eq!(
            stats.total,
            MAX_ROUTES as u64 + 2,
            "everything stays counted"
        );
        // Including the request total, which serves as a denominator: the
        // route too many has no row of its own in the table, but it was indeed
        // a request.
        assert_eq!(stats.requests, MAX_ROUTES as u64 + 2);
    }

    #[test]
    fn two_reads_of_the_same_log_give_the_same_report() {
        // A hash map does not enumerate twice in the same order. As long as
        // the quantiles came out of an exact sort, two routes tying was rare;
        // since the histogram it is the rule — and the report changed order
        // from one run to the next, which forbids comparing yesterday's with
        // today's.
        let read_once = || {
            let mut stats = stats();
            for i in 0..50 {
                let route = distinct_name(i);
                let line = format!(
                    r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}","duration_ms":120}} []"#
                );
                ingest_line(&mut stats, &line);
            }
            stats.finalize();
            let doc: Value =
                serde_json::from_str(&render_json(&stats, 0, false)).expect("some JSON");
            (render_summary(&stats), doc["endpoints"].clone())
        };

        let (summary, endpoints) = read_once();
        let (again, same_again) = read_once();
        assert_eq!(endpoints.as_array().expect("a list").len(), 50);
        assert_eq!(endpoints, same_again, "the JSON order must be reproducible");
        assert_eq!(summary, again, "the summary order too");
    }

    #[test]
    fn the_status_says_what_the_level_leaves_unsaid() {
        // A 500 caught and then logged at `info` counts as no error in the
        // sense of the level — and one it is. Conversely, a hundred 404s on
        // /favicon.ico are not outages.
        let mut aggregate = stats();
        let line = |route: &str, niveau: &str, status: u16| {
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.{niveau}: Request finished {{"route":"{route}","status":{status},"duration_ms":10}} []"#
            )
        };
        ingest_line(&mut aggregate, &line("app_orders", "INFO", 500));
        ingest_line(&mut aggregate, &line("app_home", "INFO", 404));
        ingest_line(&mut aggregate, &line("app_home", "INFO", 200));

        assert_eq!(aggregate.errors_total(), 0, "no line is of error level");
        assert_eq!(aggregate.by_status[4], 1, "one 5xx");
        assert_eq!(aggregate.by_status[3], 1, "one 4xx");
        assert_eq!(aggregate.by_status[1], 1, "one 2xx");
        assert_eq!(aggregate.responses(), 3);
        assert_eq!(aggregate.rate_5xx(), Some(1.0 / 3.0));

        assert_eq!(aggregate.routes["app_orders"].status_5xx, 1);
        assert_eq!(aggregate.routes["app_home"].status_5xx, 0);
        assert_eq!(aggregate.routes["app_home"].status_4xx, 1);
        assert_eq!(aggregate.routes["app_home"].rate_5xx(), Some(0.0));

        // With no status read at all, the question has no answer.
        let silent = stats();
        assert_eq!(silent.rate_5xx(), None);
    }

    #[test]
    fn the_quantiles_cover_everything_that_was_read() {
        // The sliding sample kept only the last 1024 durations: the fifty slow
        // requests of the morning vanished as soon as a thousand fast ones had
        // followed, and the end-of-day report kept no trace of them — while
        // `max_ms` could still see them.
        let mut route = RouteStat::default();
        for _ in 0..50 {
            route.add_duration(5000.0);
        }
        for _ in 0..1200 {
            route.add_duration(10.0);
        }

        let q = route.quantiles();
        assert!((q.p50 - 10.0).abs() / 10.0 < 0.016, "p50 = {}", q.p50);
        assert!((q.p95 - 10.0).abs() / 10.0 < 0.016, "p95 = {}", q.p95);
        assert!(
            (q.p99 - 5000.0).abs() / 5000.0 < 0.016,
            "p99 = {} — the morning's slow ones are lost",
            q.p99
        );
        assert_eq!(route.max_ms, 5000.0);
    }

    #[test]
    fn the_histogram_bounds_its_error_at_every_scale() {
        // One slice per thirty-second of an octave: the error is bounded in
        // proportion, not in milliseconds. That is what covers six orders of
        // magnitude with 672 counters.
        for value in [0.1f32, 1.0, 7.5, 120.0, 999.0, 4200.0, 60_000.0] {
            let mut route = RouteStat::default();
            route.add_duration(f64::from(value));
            let read_back = route.quantiles().p50;
            assert!(
                (read_back - value).abs() / value < 0.016,
                "{value} ms read_back {read_back} ms"
            );
        }

        // Out of bounds on both sides: everything stays counted, and the
        // maximum stays exact — it is what you read when the tail leaves the scale.
        let mut route = RouteStat::default();
        route.add_duration(0.001);
        route.add_duration(500_000.0);
        assert_eq!(route.timed, 2);
        assert_eq!(route.max_ms, 500_000.0);
        assert!(route.quantiles().p99 > 100_000.0);
    }

    #[test]
    fn a_reached_ceiling_shows_in_the_summary_and_in_the_json() {
        // The partial figure is worse than the missing one: past the ceiling
        // the endpoint table no longer shows everyone, and nothing allowed you
        // to suspect it.
        let mut saturated = stats();
        for i in 0..=MAX_ROUTES {
            ingest_line(&mut saturated, &route_line(&distinct_name(i)));
        }

        let summary = render_summary(&saturated);
        assert!(
            summary.contains("capped   : routes — new keys no longer detailed"),
            "{summary}"
        );

        let json: Value =
            serde_json::from_str(&render_json(&saturated, 25, false)).expect("some JSON");
        assert_eq!(json["capped"], json!(["routes"]));
        // The counters, themselves, have lost nothing.
        assert_eq!(json["totals"]["entries"], json!(MAX_ROUTES + 1));

        // And as long as no ceiling is reached, the list stays empty.
        let mut calm = stats();
        ingest_line(&mut calm, &route_line("app_home"));
        assert!(!calm.capped.any());
        assert!(!render_summary(&calm).contains("capped"));
    }

    #[test]
    fn the_per_request_rate_does_not_move_when_a_chatty_file_is_added() {
        // The README advises handing over `doctrine.log` as well to detect
        // N+1 patterns: dozens of DEBUG lines per HTTP request. The rate over
        // lines then collapses — same incident, threshold gone silent. The one
        // over requests does not move a hair.
        let error_line = r#"[2026-09-09T10:00:00.000000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boum: "nope" at /var/www/src/X.php line 12 {} []"#;
        let sql = r#"[2026-09-09T10:00:00.000000+02:00] doctrine.DEBUG: SELECT {"sql":"SELECT t0.id FROM client t0 WHERE t0.id = ?"} []"#;

        let fill = |stats: &mut Stats, chatter: usize| {
            for _ in 0..10 {
                ingest_line(stats, &route_line("app_home"));
                for _ in 0..chatter {
                    ingest_line(stats, sql);
                }
            }
            ingest_line(stats, error_line);
        };

        let mut alone = stats();
        fill(&mut alone, 0);
        let mut with_doctrine = stats();
        fill(&mut with_doctrine, 10);

        assert_eq!(alone.requests, 10);
        assert_eq!(with_doctrine.requests, 10, "SQL lines are not requests");
        assert_eq!(alone.request_error_rate(), Some(0.1));
        assert_eq!(with_doctrine.request_error_rate(), Some(0.1));

        // The rate over lines, itself, has been divided by ten.
        let per_line = |s: &Stats| s.errors_total() as f64 / s.total as f64;
        assert!(per_line(&alone) > 0.09, "{}", per_line(&alone));
        assert!(
            per_line(&with_doctrine) < 0.01,
            "{}",
            per_line(&with_doctrine)
        );
    }

    #[test]
    fn the_error_signature_ceiling_stops_the_table_without_stopping_the_counters() {
        let mut stats = stats();
        let error_line = |suffixe: &str| {
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom{suffixe}: "nope" at /var/www/src/X.php line 12 {{}} []"#
            )
        };
        for i in 0..MAX_ERRORS {
            ingest_line(&mut stats, &error_line(&distinct_name(i)));
        }
        assert_eq!(stats.errors.len(), MAX_ERRORS);

        let before = stats.errors_total();

        // One more unknown signature does not enter the table…
        ingest_line(&mut stats, &error_line("DeTrop"));
        assert!(stats.capped.errors, "the refusal must show");
        assert_eq!(stats.errors.len(), MAX_ERRORS, "nothing more gets in");
        // …but the error stays counted in the total. That is the distinction
        // that matters: we stop detailing, we do not stop counting, and the
        // dashboard keeps announcing the right number of errors.
        assert_eq!(
            stats.errors_total(),
            before + 1,
            "counted without being detailed"
        );

        // And a signature already known keeps accumulating.
        ingest_line(&mut stats, &error_line(&distinct_name(0)));
        assert_eq!(stats.errors_total(), before + 2);
        assert_eq!(
            stats.errors.values().filter(|stat| stat.count == 2).count(),
            1,
            "a single signature was seen twice"
        );
    }

    #[test]
    fn the_channel_ceiling_stops_the_table_without_stopping_the_counters() {
        let mut stats = stats();
        let sur_canal = |canal: &str| {
            format!("[2026-09-09T10:00:00.000000+02:00] {canal}.INFO: coucou {{}} []")
        };
        for i in 0..MAX_CHANNELS {
            ingest_line(&mut stats, &sur_canal(&distinct_name(i)));
        }
        assert_eq!(stats.channels.len(), MAX_CHANNELS);

        ingest_line(&mut stats, &sur_canal("canal_de_trop"));
        assert_eq!(stats.channels.len(), MAX_CHANNELS);
        ingest_line(&mut stats, &sur_canal(&distinct_name(0)));
        assert_eq!(stats.channels[&distinct_name(0)].count, 2);
    }

    #[test]
    fn the_sql_shape_ceiling_stops_keeping_the_text_without_stopping_counting() {
        let mut stats = stats();
        let request_lines = |table: &str| {
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT id FROM {table}"}} {{"token":"aaa"}}"#
            )
        };
        for i in 0..MAX_SQL_SHAPES {
            ingest_line(&mut stats, &request_lines(&distinct_name(i)));
        }
        assert_eq!(stats.sql_shapes(), MAX_SQL_SHAPES);

        // Beyond it the text is no longer kept — but the line is parsed all
        // the same, and the query counted in the request containing it.
        let before = stats.total;
        ingest_line(&mut stats, &request_lines("table_de_trop"));
        assert_eq!(stats.sql_shapes(), MAX_SQL_SHAPES, "no more text kept");
        assert_eq!(stats.total, before + 1, "the line stays counted");
    }

    #[test]
    fn the_nplus1_pattern_ceiling_stops_the_table() {
        let mut stats = stats();
        // One endpoint per pattern, each with its query repeated twelve
        // times — above the threshold of ten.
        let poser = |stats: &mut Stats, i: usize| {
            let token = format!("t{}", distinct_name(i));
            let endpoint = distinct_name(i);
            ingest_line(
                stats,
                &format!(
                    r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{endpoint}". {{"route":"{endpoint}"}} {{"token":"{token}"}}"#
                ),
            );
            for _ in 0..12 {
                ingest_line(
                    stats,
                    &format!(
                        r#"[2026-09-09T10:00:00.000000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT id FROM {endpoint}"}} {{"token":"{token}"}}"#
                    ),
                );
            }
        };
        for i in 0..MAX_NPLUS1 {
            poser(&mut stats, i);
        }
        stats.finalize();
        assert_eq!(stats.nplus1.len(), MAX_NPLUS1, "the table is full");

        poser(&mut stats, MAX_NPLUS1 + 1);
        stats.finalize();
        assert_eq!(stats.nplus1.len(), MAX_NPLUS1, "no more patterns kept");
    }

    #[test]
    fn the_open_request_ceiling_refuses_new_tokens() {
        let mut stats = stats();
        let avec_token = |token: &str| {
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {{"route":"app_home"}} {{"token":"{token}"}}"#
            )
        };
        for i in 0..MAX_OPEN_REQUESTS {
            ingest_line(&mut stats, &avec_token(&distinct_name(i)));
        }
        assert_eq!(stats.tracker.open_count(), MAX_OPEN_REQUESTS);

        // One more unknown identifier opens nothing: without this ceiling, a
        // token that never closes would swell the table without end.
        ingest_line(&mut stats, &avec_token("token_de_trop"));
        assert_eq!(stats.tracker.open_count(), MAX_OPEN_REQUESTS);

        // A request already open, however, keeps being followed.
        let before = stats.total;
        ingest_line(&mut stats, &avec_token(&distinct_name(0)));
        assert_eq!(stats.tracker.open_count(), MAX_OPEN_REQUESTS);
        assert_eq!(stats.total, before + 1);
    }

    #[test]
    fn the_time_axis_takes_two_dates_months_apart() {
        // Without the `min(len)` in `Timeline::record`, chaining two files
        // dated months apart would spin the cleaning loop millions of times.
        // The test passes in a blink, or not at all.
        let mut timeline = Timeline::new(600);
        timeline.record(1_757_000_000, false);
        timeline.record(1_757_000_000 + 90 * 24 * 3600, true);

        // The window has swung entirely onto the second date…
        assert_eq!(timeline.series(600, |b| b.total).iter().sum::<u64>(), 1);
        // … but the peak does not forget: on a tie it keeps the first second
        // where it was reached.
        assert_eq!(timeline.peak(), (1, 1_757_000_000));
    }

    #[test]
    fn the_peak_is_the_whole_files_not_the_last_windows() {
        // The post-mortem case: the peak happened an hour before the end of
        // the file, far beyond the ten minutes the ring keeps. As long as the
        // peak was reread from the buckets it was lost — 200 lines/s reported
        // as 1.
        let mut timeline = Timeline::new(600);
        let pointe = 1_757_000_000;
        for _ in 0..200 {
            timeline.record(pointe, false);
        }
        for i in 0..5 {
            timeline.record(pointe + 3600 + i, false);
        }

        assert_eq!(timeline.peak(), (200, pointe));
        // The ring, itself, has indeed forgotten: it shows the last five only.
        assert_eq!(timeline.series(600, |b| b.total).iter().sum::<u64>(), 5);
    }

    #[test]
    fn the_window_drops_lines_before_counting_them() {
        let mut aggregate = Stats::new(&Cli::parse_from([
            "refrain",
            "--since",
            "2026-09-09T10:00:00+02:00",
            "--until",
            "2026-09-09T10:00:01+02:00",
            "prod.log",
        ]));

        for (horodatage, niveau) in [
            ("09:59:59.999999", "CRITICAL"), // one second too early
            ("10:00:00.500000", "INFO"),     // inside the window
            ("10:00:02.000000", "CRITICAL"), // one second too late
        ] {
            let line =
                format!(r#"[2026-09-09T{horodatage}+02:00] request.{niveau}: Coucou {{}} []"#);
            aggregate.ingest(0, parse_line(&line).expect("valid line"));
        }

        assert_eq!(aggregate.total, 1, "a single line inside the window");
        assert_eq!(aggregate.out_of_window, 2);
        assert_eq!(aggregate.skipped, 0, "out of window is not unreadable");
        // Dropped errors must weigh neither on the levels nor on the time
        // axis: otherwise `--since` would return an error rate computed over
        // something other than the window asked for.
        assert_eq!(aggregate.errors_total(), 0);
        assert_eq!(aggregate.by_level[Level::Critical.index()], 0);
        assert_eq!(aggregate.timeline.peak().0, 1);
        assert!(aggregate.windowed());
        assert!(!stats().windowed(), "no bound, no window");
    }

    /// A typical Symfony request. `duration` decides whether the duration
    /// field sits on the final line, to exercise both measurement modes.
    fn request_lines(token: &str, duration: bool) -> Vec<LogEntry> {
        let end = if duration {
            r#"{"route":"app_home","method":"GET","status":200,"duration_ms":120.0}"#
        } else {
            r#"{"route":"app_home","method":"GET","status":200}"#
        };
        [
            format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {{"route":"app_home","request_uri":"https://x.test/","method":"GET"}} {{"token":"{token}"}}"#
            ),
            format!(
                r#"[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT 1"}} {{"token":"{token}"}}"#
            ),
            format!(
                r#"[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished {end} {{"token":"{token}"}}"#
            ),
        ]
        .iter()
        .map(|line| parse_line(line).expect("valid line"))
        .collect()
    }

    #[test]
    fn the_timeline_counts_per_second_and_spots_the_peak() {
        let mut timeline = Timeline::new(10);
        timeline.record(1000, false);
        timeline.record(1000, true);
        timeline.record(1002, false);

        assert_eq!(timeline.series(3, |b| b.total), vec![2, 0, 1]);
        assert_eq!(timeline.series(3, |b| b.errors), vec![1, 0, 0]);
        assert_eq!(timeline.peak(), (2, 1000));

        // An entry older than the window is ignored, without panicking.
        timeline.record(1, false);
        assert_eq!(timeline.peak().0, 2);
    }

    #[test]
    fn duration_units_are_inferred() {
        let ms = |v, key| to_millis(&v, key, DurationUnit::Auto);
        assert_eq!(ms(json!(150), "duration_ms"), Some(150.0));
        // PHP measures in floating seconds: 0.25 s = 250 ms.
        assert_eq!(ms(json!(0.25), "duration"), Some(250.0));
        assert_eq!(ms(json!("1.5s"), "elapsed"), Some(1500.0));
        assert_eq!(ms(json!(2000), "elapsed_us"), Some(2.0));
        // An integer with no suffix stays milliseconds.
        assert_eq!(ms(json!(430), "duration"), Some(430.0));
        assert_eq!(ms(json!("bonjour"), "duration"), None);
    }

    #[test]
    fn a_duration_field_wins_over_correlation() {
        let mut stats = stats();
        for entry in request_lines("aaa", true) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.total, 3);
        let route = &stats.routes["app_home"];
        assert_eq!(route.requests, 1, "a single request counted");
        assert_eq!(
            route.timed, 1,
            "no double measurement, field plus correlation"
        );
        assert!((route.max_ms - 120.0).abs() < 0.01);
        assert!(matches!(stats.duration, DurationSource::Field { .. }));
    }

    #[test]
    fn without_a_duration_field_correlation_takes_over() {
        let mut stats = stats();
        for entry in request_lines("bbb", false) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let route = &stats.routes["app_home"];
        assert_eq!(route.requests, 1);
        assert_eq!(route.timed, 1, "duration deduced from the token");
        // First line at .000, last at .120: 120 ms.
        assert!(
            (route.max_ms - 120.0).abs() < 1.0,
            "duration = {}",
            route.max_ms
        );
        assert!(matches!(stats.duration, DurationSource::Correlated { .. }));
    }

    #[test]
    fn an_error_with_no_route_context_is_attached_by_its_token() {
        let mut stats = stats();
        let mut entries = request_lines("ccc", false);
        // The exception line carries only the exception: no route.
        let error_line = parse_line(
            r#"[2026-09-09T10:00:00.100000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\\Exception\\Boom(code: 0): nope at /var/www/src/X.php:12)"} {"token":"ccc"}"#,
        )
        .unwrap();
        entries.insert(2, error_line);
        for entry in entries {
            stats.ingest(0, entry);
        }

        assert_eq!(stats.errors_total(), 1);
        let (_, error_line) = stats.errors.iter().next().unwrap();
        assert_eq!(error_line.exception.as_deref(), Some(r"App\Exception\Boom"));
        assert_eq!(error_line.endpoint.as_deref(), Some("app_home"));
        assert_eq!(stats.routes["app_home"].errors, 1);
    }

    /// A Doctrine line: prepared statement, parameters kept apart.
    fn sql_line(token: &str, sql: &str) -> LogEntry {
        let line = format!(
            r#"[2026-09-09T10:00:00.060000+02:00] doctrine.DEBUG: Executing statement {{"sql":"{sql}","params":{{"1":1}}}} {{"token":"{token}"}}"#
        );
        parse_line(&line).expect("valid SQL line")
    }

    /// Inserts `n` SQL queries in the middle of a typical HTTP request.
    fn request_with_sql(token: &str, sql: impl Fn(usize) -> String, n: usize) -> Vec<LogEntry> {
        let mut entries = request_lines(token, true);
        for i in 0..n {
            entries.insert(2, sql_line(token, &sql(i)));
        }
        entries
    }

    #[test]
    fn an_nplus1_is_detected() {
        let mut stats = stats();
        // The faulty loop: exactly the same prepared statement twelve times.
        let sql = "SELECT t0.id, t0.name FROM product t0 WHERE t0.id = ?";
        for entry in request_with_sql("nnn", |_| sql.to_string(), 12) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.nplus1.len(), 1, "un alone pattern attendu");
        let pattern = stats.nplus1.values().next().unwrap();
        assert_eq!(pattern.endpoint, "app_home");
        assert_eq!(pattern.max_count, 12);
        assert_eq!(pattern.requests, 1);
        assert!(pattern.sql.contains("FROM product"));
        // Twelve repetitions plus the "SELECT 1" of the typical request.
        assert_eq!(stats.routes["app_home"].queries_max, 13);
    }

    #[test]
    fn varied_queries_trigger_nothing() {
        let mut stats = stats();
        for entry in request_with_sql("vvv", |i| format!("SELECT id FROM table_{i}"), 30) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert!(
            stats.nplus1.is_empty(),
            "thirty distinct queries do not make an N+1"
        );
        assert_eq!(stats.routes["app_home"].queries_max, 31);
        assert_eq!(stats.sql_shapes(), 31);
    }

    #[test]
    fn a_pattern_accumulates_across_requests() {
        let mut stats = stats();
        let sql = "SELECT t0.id FROM address t0 WHERE t0.customer_id = ?";
        for (token, n) in [("a", 15), ("b", 40)] {
            for entry in request_with_sql(token, |_| sql.to_string(), n) {
                stats.ingest(0, entry);
            }
        }
        stats.finalize();

        let pattern = stats.nplus1.values().next().expect("pattern detected");
        assert_eq!(pattern.requests, 2, "seen across two HTTP requests");
        assert_eq!(pattern.max_count, 40, "the worst case is kept");
        assert!((pattern.avg_count() - 27.5).abs() < 0.01);
    }

    #[test]
    fn a_zero_threshold_disables_detection() {
        let mut cli = Cli::parse_from(["refrain", "prod.log"]);
        cli.nplus1 = 0;
        let mut stats = Stats::new(&cli);

        for entry in request_with_sql("zzz", |_| "SELECT 42".to_string(), 50) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert!(stats.nplus1.is_empty());
        // The SQL counting, itself, carries on.
        assert_eq!(stats.routes["app_home"].queries_max, 51);
    }

    /// Two files read in parallel do not advance through time at the same
    /// speed: `prod.log` is short, so its reader runs much further than
    /// `doctrine.log`'s. The sweep must not pace itself on the faster one,
    /// otherwise it cuts the slower one's requests into pieces — and an N+1
    /// split across two pieces never crosses the threshold again.
    #[test]
    fn one_sources_lead_does_not_cut_anothers_requests() {
        let cli = Cli::parse_from(["refrain", "prod.log", "doctrine.log"]);
        let mut stats = Stats::new(&cli);
        let sql = "SELECT t0.id FROM address t0 WHERE t0.customer_id = ?";

        // prod.log (source 0) opens the request.
        stats.ingest(0, request_lines("xyz", true).remove(0));
        // doctrine.log (source 1) delivers half of its SQL queries.
        for _ in 0..6 {
            stats.ingest(1, sql_line("xyz", sql));
        }
        // prod.log runs five minutes further: its reader is ahead.
        let plus_loin =
            parse_line(r#"[2026-09-09T10:05:00.000000+02:00] app.INFO: autre chose [] []"#)
                .expect("valid line");
        stats.ingest(0, plus_loin);
        // doctrine.log, left behind, delivers the rest of the same request.
        for _ in 0..6 {
            stats.ingest(1, sql_line("xyz", sql));
        }
        stats.finalize();

        let pattern = stats
            .nplus1
            .values()
            .next()
            .expect("the twelve executions form a single N+1");
        assert_eq!(
            pattern.max_count, 12,
            "one HTTP request, the same SQL query twelve times"
        );
        assert_eq!(pattern.requests, 1);
        // The twelve executions counted on one HTTP request, not two halves
        // of six.
        assert_eq!(stats.routes["app_home"].queries_max, 12);
    }

    #[test]
    fn the_idle_sweep_waits_for_a_source_still_behind_in_its_file() {
        // The dashboard sweeps on the wall clock when a tick goes by with
        // nothing arriving. Read from the start of yesterday's log, a reader
        // stalled a quarter of a second on a slow disk used to see every open
        // request closed under it, and the lines that followed opened them
        // again as new ones: one request became two.
        let mut stats = stats();
        for entry in request_lines("idle", false) {
            stats.ingest(0, entry);
        }
        assert_eq!(stats.tracker.open_count(), 1);
        assert_eq!(
            stats.sweep_idle(),
            0,
            "the reader has not reached the end of its file: the request may still receive lines"
        );

        // Caught up: the source is on wall-clock time, and the request, dated
        // long ago, is over.
        stats.source_caught_up(0);
        assert_eq!(stats.sweep_idle(), 1);
        assert_eq!(stats.routes["app_home"].timed, 1);

        // With two sources, one of them silent so far, nothing is closed
        // either: it may hold lines of any date.
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log", "doctrine.log"]));
        for entry in request_lines("pair", false) {
            stats.ingest(0, entry);
        }
        stats.source_caught_up(0);
        assert_eq!(
            stats.sweep_idle(),
            0,
            "doctrine.log has delivered nothing yet"
        );
        stats.ingest(
            1,
            parse_line(r#"[2026-09-09T10:00:00.000000+02:00] doctrine.DEBUG: SELECT 1 {"sql":"SELECT 1"} {"token":"pair"}"#)
                .expect("valid line"),
        );
        assert_eq!(
            stats.sweep_idle(),
            0,
            "doctrine.log stands at the request's own date"
        );
        stats.source_done(1);
        assert_eq!(
            stats.sweep_idle(),
            1,
            "closed for good: only prod.log's clock counts"
        );
    }

    #[test]
    fn the_json_output_exposes_the_expected_metrics() {
        let mut stats = stats();
        for entry in request_lines("aaa", true) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let doc: Value =
            serde_json::from_str(&render_json(&stats, 25, false)).expect("well-formed JSON");

        assert_eq!(doc["totals"]["entries"], 3);
        assert_eq!(doc["totals"]["errors"], 0);
        assert_eq!(doc["levels"]["info"], 2);
        assert_eq!(doc["levels"]["debug"], 1);
        assert_eq!(doc["duration_source"]["kind"], "field");
        assert_eq!(doc["duration_source"]["key"], "duration_ms");

        let endpoint = &doc["endpoints"][0];
        assert_eq!(endpoint["endpoint"], "app_home");
        assert_eq!(endpoint["requests"], 1);
        // The quantile comes out of a histogram: it is the slice's value, to
        // within ±1.6 %. The maximum, itself, is tracked exactly.
        let p95 = endpoint["p95_ms"].as_f64().expect("a number");
        assert!((p95 - 120.0).abs() / 120.0 < 0.016, "p95 = {p95}");
        assert_eq!(endpoint["max_ms"], 120.0);
    }

    #[test]
    fn the_top_option_limits_the_lists() {
        let mut stats = stats();
        for route in ["a", "b", "c"] {
            let line = format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}","duration_ms":10}} []"#
            );
            stats.ingest(0, parse_line(&line).unwrap());
        }

        let combien = |top| {
            let doc: Value = serde_json::from_str(&render_json(&stats, top, false)).unwrap();
            doc["endpoints"].as_array().unwrap().len()
        };
        assert_eq!(combien(2), 2);
        assert_eq!(combien(0), 3, "0 signifie « tous »");
    }

    #[test]
    fn large_numbers_are_readable() {
        assert_eq!(format_count(1234567), "1,234,567");
        assert_eq!(format_count(42), "42");
        assert_eq!(format_ms(1500.0), "1.50 s");
        assert_eq!(format_ms(12.34), "12 ms");
    }
}
