//! Aggregation: this is where a stream of lines becomes useful figures.
//!
//! Every structure in this module is designed for a **fixed memory bound**: 40
//! GB of logs can be swallowed without the footprint moving. The quantiles rest
//! on a bounded-error histogram, the time axis on a ring buffer, and the
//! grouping tables have a ceiling.

use crate::cli::{Cli, DurationUnit};
use crate::parser::{
    CacheEvent, CommandLine, HttpCall, Level, LogEntry, MessageEvent, MessengerLine,
};
use chrono::{DateTime, FixedOffset, Local, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Ceilings: beyond them, we stop adding new keys (the ones already known keep
/// being counted). Without this, an identifier slipping into a route would blow
/// the memory up.
const MAX_ROUTES: usize = 4096;
const MAX_ERRORS: usize = 4096;
/// Distinct deprecations detailed — the same bound as error signatures, for
/// the same reason: the key comes out of a message.
const MAX_DEPRECATIONS: usize = 4096;
const MAX_CHANNELS: usize = 512;
/// Subjects listed for one error signature or one deprecation. Past it the row
/// says "16+", which answers the same question any larger number would: this
/// one is everywhere.
const MAX_SUBJECTS_PER_KEY: usize = 16;
const MAX_OPEN_REQUESTS: usize = 20_000;
/// SQL query shapes whose text is kept.
const MAX_SQL_SHAPES: usize = 2048;
/// Outbound HTTP call shapes whose figures are kept. The same bound as the SQL
/// shapes, for the same reason: the key comes out of a URL, and a URL is where
/// an unbounded identifier is guaranteed to show up.
const MAX_HTTP_SHAPES: usize = 2048;
/// Message classes on the bus whose figures are kept. A class name comes from
/// the logs like every other key here, and so takes the same bound.
const MAX_MESSAGE_CLASSES: usize = 2048;
/// Messages dispatched and still waiting for the line that handles them. Its
/// own ceiling, separate from the open requests: a dead consumer leaves tens
/// of thousands of them, and only the **lag** stops being measured past it —
/// how many are waiting is arithmetic on two counters, which no ceiling
/// touches.
const MAX_OPEN_MESSAGES: usize = 20_000;
/// Console commands detailed. A codebase has tens of commands, not thousands
/// — but the name still comes from the logs, so it still takes a ceiling.
const MAX_COMMANDS: usize = 512;
/// Cache keys whose misses are detailed. The key comes from the logs, folded
/// the way an error signature is, and takes the same bound as the rest.
const MAX_CACHE_KEYS: usize = 2048;
/// Distinct N+1 patterns followed (endpoint × SQL query pairs).
const MAX_NPLUS1: usize = 1024;
/// Distinct shapes — SQL queries, outbound calls, messages dispatched —
/// followed within one HTTP request: beyond this, the total keeps being
/// counted without memorising new shapes.
const MAX_SHAPES_PER_REQUEST: usize = 256;

/// Channels whose error lines cannot come from an HTTP request. A failing cron
/// job is not a failing endpoint: counting it against requests makes
/// `request-error-rate` move with whatever else happens to sit in the file —
/// the one thing that metric exists not to do.
///
/// The list is deliberately short: it holds what Symfony names without
/// ambiguity. A command and a worker also raise their exceptions on the
/// application's own channels, and those keep being counted until each has a
/// subject of its own.
const OFF_REQUEST_CHANNELS: [&str; 1] = ["console"];

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
    pub deprecations: bool,
    pub channels: bool,
    pub sql_shapes: bool,
    pub nplus1: bool,
    pub http_shapes: bool,
    pub message_classes: bool,
    pub open_messages: bool,
    pub cache_keys: bool,
    pub commands: bool,
    pub open_requests: bool,
}

impl Capped {
    /// The saturated tables, under the name the user knows them by.
    pub fn names(self) -> Vec<&'static str> {
        [
            (self.routes, "routes"),
            (self.errors, "errors"),
            (self.deprecations, "deprecations"),
            (self.channels, "channels"),
            (self.sql_shapes, "sql shapes"),
            (self.nplus1, "n+1 patterns"),
            (self.http_shapes, "outbound calls"),
            (self.message_classes, "message classes"),
            (self.open_messages, "open messages"),
            (self.cache_keys, "cache keys"),
            (self.commands, "commands"),
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

/// Span of seconds whose count is kept exactly, for the peak: 2^22 s is 48
/// days, 16 MB at the very most — and a real log's seconds are contiguous, a
/// day costing 340 KB. Past the span, the ring is what remains, as before.
const MAX_EXACT_SPAN: u64 = 1 << 22;

/// A ring buffer of one bucket per second: that is what gives the sparklines
/// and the sliding rates.
///
/// The principle: `head` designates the bucket of the most recent second. When
/// a more recent entry arrives, `head` advances, zeroing the buckets crossed on
/// the way. Nothing is ever allocated after construction.
///
/// The peak does not come out of the ring. The ring forgets — a post-mortem
/// file covers hours, the ring ten minutes — and, above all, the sources race:
/// each file is read by its own thread, and when `prod.log` has reached the end
/// of the half hour while `doctrine.log` is still at its start, every Doctrine
/// line behind the ring is dropped from it. The same 239,556 lines gave a peak
/// of 547 lines/s in one file and 334 in two, depending on which thread ran
/// ahead. So every second read is also counted exactly, in a dense table that
/// no lead between sources can push a line out of.
pub struct Timeline {
    buckets: Vec<Bucket>,
    head: usize,
    head_epoch: i64,
    started: bool,
    /// The busiest second seen since the start, and its epoch.
    peak: (u64, i64),
    /// One exact counter per second read, from `origin` on.
    seconds: VecDeque<u32>,
    origin: i64,
    /// The second being counted, and its total so far. Lines come in runs of
    /// the same second, so the table is only touched when the second changes:
    /// once a second on a live stream, once a batch when sources interleave.
    current: Option<(i64, u32)>,
}

impl Timeline {
    pub fn new(seconds: usize) -> Self {
        Self {
            buckets: vec![Bucket::default(); seconds],
            head: 0,
            head_epoch: 0,
            started: false,
            peak: (0, 0),
            seconds: VecDeque::new(),
            origin: 0,
            current: None,
        }
    }

    pub fn record(&mut self, epoch: i64, is_error: bool) {
        // Exact first: the peak is read from here, and it must not depend on
        // where the ring stands.
        if let Some(count) = self.count_exactly(epoch)
            && count > self.peak.0
        {
            self.peak = (count, epoch);
        }

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
        // Past the exact span, the ring is what remains for the peak: one
        // `max` per line, where sweeping the ring cost 600 comparisons on every
        // read of it.
        if bucket.total > self.peak.0 {
            self.peak = (bucket.total, epoch);
        }
    }

    /// Counts the line in its second, exactly, and returns the second's new
    /// total — or `None` if that second lies outside the span kept.
    ///
    /// The table is dense from `origin`: a second before it is reached by
    /// growing at the front, a second after it by growing at the back, and
    /// the seconds in between, empty, cost their four bytes each. That is
    /// what makes a day 340 KB and a lookup two subtractions.
    fn count_exactly(&mut self, epoch: i64) -> Option<u64> {
        if let Some((second, count)) = &mut self.current
            && *second == epoch
        {
            *count = count.saturating_add(1);
            return Some(u64::from(*count));
        }
        // The second changes: the one just left goes back to the table, the
        // new one is loaded from it — it may already hold another source's
        // share.
        if let Some((second, count)) = self.current.take() {
            let index = (second - self.origin) as usize;
            self.seconds[index] = count;
        }
        let index = self.slot(epoch)?;
        let count = self.seconds[index].saturating_add(1);
        self.current = Some((epoch, count));
        Some(u64::from(count))
    }

    /// The table index of a second, growing the table to reach it — or `None`
    /// if that would take it past the span kept.
    fn slot(&mut self, epoch: i64) -> Option<usize> {
        if self.seconds.is_empty() {
            self.origin = epoch;
            self.seconds.push_back(0);
        }
        let offset = epoch - self.origin;
        if offset < 0 {
            let gap = offset.unsigned_abs();
            if gap + self.seconds.len() as u64 > MAX_EXACT_SPAN {
                return None;
            }
            for _ in 0..gap {
                self.seconds.push_front(0);
            }
            self.origin = epoch;
            return Some(0);
        }
        if offset as u64 >= MAX_EXACT_SPAN {
            return None;
        }
        let index = offset as usize;
        if index >= self.seconds.len() {
            self.seconds.resize(index + 1, 0);
        }
        Some(index)
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
    /// second), whatever the number of files it sits in. In the dashboard,
    /// "everything" starts over at `r`, which rebuilds the aggregate.
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
    /// The latest subject to raise it — a route, or a console command. Bare
    /// because it used to be what the follow matched on; with the method
    /// glued on, "GET app_checkout" never equalled "app_checkout".
    pub endpoint: Option<String>,
    /// The verb, kept apart so it can be shown without being matched on.
    pub method: Option<String>,
    /// **Every** subject seen raising it, which is what the follow matches.
    ///
    /// One signature is raised from six routes as often as from one, and the
    /// row above remembers only the last of them — so following any of the
    /// other five used to list nothing. That a signature is raised from six
    /// routes is also the interesting thing about it, and until now refrain
    /// could not say so.
    pub subjects: Subjects,
}

/// The subjects that raised one key, bounded.
#[derive(Default, Clone)]
pub struct Subjects {
    names: std::collections::BTreeSet<String>,
    /// The ceiling turned one away: the list below is a subset.
    pub capped: bool,
}

impl Subjects {
    fn insert(&mut self, name: &str) {
        if self.names.contains(name) {
            return;
        }
        if self.names.len() >= MAX_SUBJECTS_PER_KEY {
            self.capped = true;
            return;
        }
        self.names.insert(name.to_string());
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Sorted, because a `BTreeSet` is: two reads of one log owe the same
    /// report, down to the order of this list.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// How many raised it, as a row says it: `6`, or `16+` past the ceiling.
    pub fn count(&self) -> String {
        match self.capped {
            true => format!("{}+", self.names.len()),
            false => self.names.len().to_string(),
        }
    }
}

impl ErrorStat {
    /// What raised it, as it reads: `GET app_checkout`.
    pub fn subject(&self) -> Option<String> {
        let endpoint = self.endpoint.as_deref()?;
        Some(match &self.method {
            Some(method) => format!("{method} {endpoint}"),
            None => endpoint.to_string(),
        })
    }
}

/// One deprecation, as grouped under its key — message and origin, both
/// normalised (see `LogEntry::deprecation_key`).
#[derive(Clone)]
pub struct DeprecationStat {
    pub count: u64,
    pub channel: String,
    /// The latest occurrence, verbatim: the key has its identifiers erased,
    /// this is where they are read back.
    pub message: String,
    /// `path:line` of the latest occurrence, with its real line number.
    pub origin: Option<String>,
    /// The route that triggered it last. One deprecation reached from twenty
    /// routes is one row: the route is a hint about where to look, not part
    /// of the key.
    pub endpoint: Option<String>,
    /// All twenty of them, which is what the follow matches — and, for a
    /// deprecation more than anything, the measure of how far it reaches.
    pub subjects: Subjects,
    pub first_seen: Option<DateTime<FixedOffset>>,
    pub last_seen: Option<DateTime<FixedOffset>>,
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
    /// HTTP requests closed for this endpoint: denominator of the SQL and
    /// outbound-call averages.
    pub closed_requests: u64,
    pub queries_total: u64,
    pub queries_max: u32,
    /// Outbound HTTP calls made by those requests. An endpoint calling a
    /// third party four times per request is the N+1 no index will fix.
    pub calls_total: u64,
    pub calls_max: u32,
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

    /// Counts what an HTTP request that has just closed did: its SQL queries
    /// and its outbound calls. Requests that made none count too — otherwise
    /// both averages would be inflated by leaving out the quiet requests.
    fn add_request_totals(&mut self, queries: u32, calls: u32) {
        self.closed_requests += 1;
        self.queries_total += u64::from(queries);
        self.queries_max = self.queries_max.max(queries);
        self.calls_total += u64::from(calls);
        self.calls_max = self.calls_max.max(calls);
    }

    /// SQL queries per HTTP request, on average.
    pub fn avg_queries(&self) -> f32 {
        self.per_request(self.queries_total)
    }

    /// Outbound HTTP calls per HTTP request, on average.
    pub fn avg_calls(&self) -> f32 {
        self.per_request(self.calls_total)
    }

    fn per_request(&self, total: u64) -> f32 {
        if self.closed_requests == 0 {
            0.0
        } else {
            total as f32 / self.closed_requests as f32
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
// Outbound HTTP calls
// ---------------------------------------------------------------------------

/// Statistics for one outbound call shape — a verb, a host, a path.
///
/// Everything the endpoint table does, on calls that cost ten to a hundred
/// times what an SQL query does. The durations live in the same bounded-error
/// histogram, so the quantiles cover everything read.
#[derive(Default, Clone)]
pub struct HttpStat {
    /// The shape as it is displayed and grouped: `GET api.example.com/v1/x`.
    /// Never a query string — see [`crate::parser::HttpCall`].
    pub shape: String,
    pub calls: u64,
    pub timed: u64,
    pub sum_ms: f64,
    pub max_ms: f32,
    /// Calls that carried a status: the denominator of the two counters
    /// below, and not the number of calls — a line may say how long it took
    /// without saying what came back.
    pub responses: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    /// HTTP requests that made at least one call of this shape, and how many
    /// they made. That is the N+1 on a third party: not a slow provider, a
    /// provider called four times where once would do.
    pub requests: u64,
    total_per_request: u64,
    pub max_per_request: u32,
    /// The subject that made the most of them within a single run — the
    /// place the repetition is fixed. An endpoint, or a command: a cron job
    /// calls a provider in a loop as readily as a route does.
    pub worst_subject: Option<String>,
    pub last_seen: Option<DateTime<FixedOffset>>,
    /// Allocated on the first duration only: a shape read off a line that
    /// carries no `total_time` does not pay for its 672 counters.
    histogram: Option<Box<Histogram>>,
}

impl HttpStat {
    fn add_duration(&mut self, ms: f64) {
        self.timed += 1;
        self.sum_ms += ms;
        let ms = ms as f32;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        self.histogram.get_or_insert_with(Box::default).record(ms);
    }

    pub fn quantiles(&self) -> Quantiles {
        match &self.histogram {
            Some(histogram) => histogram.quantiles(self.timed),
            None => Quantiles::default(),
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.timed == 0 {
            0.0
        } else {
            (self.sum_ms / self.timed as f64) as f32
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

    /// One run closed, having made `count` calls of this shape.
    fn record_request(&mut self, count: u32, subject: &str) {
        self.requests += 1;
        self.total_per_request += u64::from(count);
        // A tie is broken by name, like every sort here. `sweep` walks a hash
        // map: two requests closing in the same pass do not arrive in the same
        // order twice, and the endpoint this names must not depend on that —
        // two reads of one log owe the same report.
        let wins = count > self.max_per_request
            || (count == self.max_per_request
                && self
                    .worst_subject
                    .as_deref()
                    .is_none_or(|current| subject < current));
        if wins {
            self.max_per_request = count;
            self.worst_subject = Some(subject.to_string());
        }
    }

    /// Calls of this shape per HTTP request that made any, on average. The
    /// denominator leaves out the requests that called elsewhere: the question
    /// is "when this provider is called, how many times", not "how often is it
    /// called at all", which `calls` already answers.
    pub fn avg_per_request(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.total_per_request as f32 / self.requests as f32
        }
    }
}

// ---------------------------------------------------------------------------
// Console commands
// ---------------------------------------------------------------------------

/// One console command, across its runs.
///
/// A command is the cron job's endpoint: a name, a number of runs, and an
/// exit code that is a status. It is kept apart from the endpoints on
/// purpose — `requests`, `request-error-rate` and the peak are all defined
/// over HTTP requests, and a command is not one. See
/// [`Stats::requests`].
#[derive(Default, Clone)]
pub struct CommandStat {
    pub name: String,
    /// Runs that ended. Counted on the line that says so, which Symfony
    /// writes exactly once per run.
    pub runs: u64,
    /// Runs that ended with a non-zero exit code.
    pub failed: u64,
    /// Exceptions logged while the command ran. Beside `failed` rather than
    /// merged into it: a command can throw, catch, and still exit zero.
    pub threw: u64,
    /// The code of the last run that ended.
    pub last_code: Option<i64>,
    /// Runs whose lines the correlation swept up, and what they did: the
    /// denominator of the two averages below. A run whose lines could not be
    /// tied together counts in `runs` and not here, so the averages stay over
    /// the runs they were actually measured on.
    pub closed_runs: u64,
    pub queries_total: u64,
    pub queries_max: u32,
    pub calls_total: u64,
    pub calls_max: u32,
    /// Durations, where the lines of a run could be tied together. A command
    /// writes nothing when it starts, so this is the gap between the first
    /// line its process wrote and the one saying it exited.
    pub timed: u64,
    sum_ms: f64,
    pub max_ms: f32,
    histogram: Option<Box<Histogram>>,
    pub first_seen: Option<DateTime<FixedOffset>>,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl CommandStat {
    fn add_duration(&mut self, ms: f64) {
        self.timed += 1;
        self.sum_ms += ms;
        let ms = ms as f32;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        self.histogram.get_or_insert_with(Box::default).record(ms);
    }

    pub fn quantiles(&self) -> Quantiles {
        match &self.histogram {
            Some(histogram) => histogram.quantiles(self.timed),
            None => Quantiles::default(),
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.timed == 0 {
            0.0
        } else {
            (self.sum_ms / self.timed as f64) as f32
        }
    }

    /// SQL queries per run, over the runs whose lines were tied together.
    /// A nightly import running four thousand of them is the N+1 nobody
    /// watches: a profiler gets opened on a route, never on a cron.
    pub fn avg_queries(&self) -> f32 {
        self.per_run(self.queries_total)
    }

    /// Outbound HTTP calls per run, over the same.
    pub fn avg_calls(&self) -> f32 {
        self.per_run(self.calls_total)
    }

    fn per_run(&self, total: u64) -> f32 {
        if self.closed_runs == 0 {
            0.0
        } else {
            total as f32 / self.closed_runs as f32
        }
    }

    /// Share of runs that ended badly. `None` when none has ended: a command
    /// still running is not a command that succeeded.
    pub fn failure_rate(&self) -> Option<f64> {
        match self.runs {
            0 => None,
            runs => Some(self.failed as f64 / runs as f64),
        }
    }
}

// ---------------------------------------------------------------------------
// Cache misses
// ---------------------------------------------------------------------------

/// Misses on one cache key.
///
/// Symfony writes a line when it **computes** an item and nothing at all when
/// it serves one from the cache, so every line counted here is a miss. A key
/// that shows up once per request is a cache that is not working — a
/// five-minute fix, and invisible until something counts these.
#[derive(Default, Clone)]
pub struct CacheStat {
    /// The key, folded the way an error signature is: `product_#_teasers`.
    pub key: String,
    /// Items this process computed.
    pub computed: u64,
    /// Times the item was already being computed elsewhere and this one
    /// waited. The lock exists to blunt a stampede; this is one happening.
    pub contended: u64,
    /// HTTP requests that missed on this key, and how many times they did.
    /// Twice within one request is the same item computed twice over.
    pub requests: u64,
    total_per_request: u64,
    pub max_per_request: u32,
    pub worst_subject: Option<String>,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl CacheStat {
    pub fn misses(&self) -> u64 {
        self.computed + self.contended
    }

    /// One run closed, having missed `count` times on this key.
    fn record_request(&mut self, count: u32, subject: &str) {
        self.requests += 1;
        self.total_per_request += u64::from(count);
        // Ties broken by name, like every sort here: `sweep` walks a hash map.
        let wins = count > self.max_per_request
            || (count == self.max_per_request
                && self
                    .worst_subject
                    .as_deref()
                    .is_none_or(|current| subject < current));
        if wins {
            self.max_per_request = count;
            self.worst_subject = Some(subject.to_string());
        }
    }

    pub fn avg_per_request(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.total_per_request as f32 / self.requests as f32
        }
    }
}

// ---------------------------------------------------------------------------
// Messages on the bus
// ---------------------------------------------------------------------------

/// Statistics for one message class on the Messenger bus.
///
/// The figure the whole dimension exists for is the simplest one here:
/// `dispatched - handled`. It needs no correlation and stops at no ceiling,
/// which is what lets refrain say "89,338 sent, 374 handled" about a consumer
/// that died on Friday evening.
#[derive(Default, Clone)]
pub struct MessageStat {
    /// Fully qualified, as the log names it: `App\Message\IndexEntityMessage`.
    pub class: String,
    /// Dispatches, counted once per vocabulary rather than once in total.
    ///
    /// An application may run Symfony's own logging **and** an audit
    /// middleware — the log that prompted this dimension does — and the same
    /// dispatch then reaches refrain twice, once as `Sending message …` and
    /// once as `[id] Sent …`. Adding them would double every figure on this
    /// row, so they are kept apart and [`MessageStat::dispatched`] takes the
    /// larger: whichever vocabulary saw more of them saw them all.
    dispatched_core: u64,
    dispatched_audit: u64,
    handled_core: u64,
    handled_audit: u64,
    /// Handler invocations. Beside `handled` because a message with two
    /// handlers runs twice for one acknowledgement — and because a
    /// synchronous dispatch writes this line and no other.
    pub runs: u64,
    pub no_handler: u64,
    pub retried: u64,
    /// Removed from the transport after its retries, or rejected to the
    /// failure transport.
    pub failed: u64,
    /// Dispatches paired with their handling by an identifier, and the lag
    /// between the two. Empty unless the application logs an id on dispatch —
    /// core Symfony does not.
    pub timed: u64,
    sum_ms: f64,
    pub max_ms: f32,
    histogram: Option<Box<Histogram>>,
    /// HTTP requests that dispatched at least one of these, and how many they
    /// dispatched: the N+1 on the bus, one layer above the SQL one and
    /// usually the costlier, since every message is a job someone must run.
    pub requests: u64,
    total_per_request: u64,
    pub max_per_request: u32,
    pub worst_subject: Option<String>,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl MessageStat {
    /// Handed to a transport. A message routed to two senders counts twice,
    /// because it really is sent twice.
    pub fn dispatched(&self) -> u64 {
        self.dispatched_core.max(self.dispatched_audit)
    }

    /// Acknowledged to the transport by a worker: off the queue.
    pub fn handled(&self) -> u64 {
        self.handled_core.max(self.handled_audit)
    }

    /// Dispatched with no line saying they were handled. Over the window read
    /// and not "for ever": a message dispatched in the file's last second is
    /// in flight, not lost, and only a read that ends long after the dispatch
    /// tells the two apart.
    pub fn waiting(&self) -> u64 {
        self.dispatched().saturating_sub(self.handled())
    }

    fn add_lag(&mut self, ms: f64) {
        self.timed += 1;
        self.sum_ms += ms;
        let ms = ms as f32;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        self.histogram.get_or_insert_with(Box::default).record(ms);
    }

    pub fn quantiles(&self) -> Quantiles {
        match &self.histogram {
            Some(histogram) => histogram.quantiles(self.timed),
            None => Quantiles::default(),
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.timed == 0 {
            0.0
        } else {
            (self.sum_ms / self.timed as f64) as f32
        }
    }

    /// One run closed, having dispatched `count` of these.
    fn record_request(&mut self, count: u32, subject: &str) {
        self.requests += 1;
        self.total_per_request += u64::from(count);
        // Ties broken by name, like every sort here: `sweep` walks a hash map
        // and two requests closing together do not arrive in the same order
        // twice.
        let wins = count > self.max_per_request
            || (count == self.max_per_request
                && self
                    .worst_subject
                    .as_deref()
                    .is_none_or(|current| subject < current));
        if wins {
            self.max_per_request = count;
            self.worst_subject = Some(subject.to_string());
        }
    }

    pub fn avg_per_request(&self) -> f32 {
        if self.requests == 0 {
            0.0
        } else {
            self.total_per_request as f32 / self.requests as f32
        }
    }

    /// The name a table shows: the class without its namespace, which is what
    /// anyone calls it. The full one stays in `class`, for the detail.
    pub fn short_name(&self) -> &str {
        crate::parser::short_class(&self.class)
    }
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

/// What one HTTP request did, of one kind: how many times each shape, and how
/// many in total.
///
/// The four kinds — SQL queries, outbound calls, messages dispatched, cache
/// items computed — are counted identically and differ only in what they
/// count, so they share the counting rather than repeating it four times.
#[derive(Default)]
struct PerRequest {
    /// Shape fingerprint → occurrences within this HTTP request.
    shapes: HashMap<u64, u32>,
    /// Total, including the shapes not memorised for lack of room.
    total: u32,
}

impl PerRequest {
    /// The total counts every time; the table stops taking new keys at the
    /// ceiling. A request running ten thousand distinct queries is a bug in
    /// the application, and it must not become one in refrain.
    fn count(&mut self, fingerprint: u64) {
        self.total = self.total.saturating_add(1);
        if self.shapes.contains_key(&fingerprint) || self.shapes.len() < MAX_SHAPES_PER_REQUEST {
            *self.shapes.entry(fingerprint).or_insert(0) += 1;
        }
    }

    /// Hands the table over without copying it: the request is destroyed
    /// right after, anyway.
    fn take(&mut self) -> Vec<(u64, u32)> {
        std::mem::take(&mut self.shapes).into_iter().collect()
    }
}

/// What one line contributed, by kind. All four are `None` for the
/// overwhelming majority of lines; they travel together because they are
/// counted together, and because four more parameters on `observe` said
/// nothing that this name does not.
#[derive(Default, Clone, Copy)]
pub struct Shapes {
    pub sql: Option<u64>,
    pub call: Option<u64>,
    pub message: Option<u64>,
    pub cache: Option<u64>,
}

/// What a run of correlated lines belongs to.
///
/// Two kinds, and the difference is not cosmetic: everything a request does
/// feeds figures defined over requests — `timed`, the endpoint table — and a
/// command must feed none of them, while both of them can run four thousand
/// queries and want that counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// An HTTP request, under the endpoint it was matched to.
    Endpoint(String),
    /// A console command, under its name.
    Command(String),
}

impl Subject {
    pub fn name(&self) -> &str {
        match self {
            Self::Endpoint(name) | Self::Command(name) => name,
        }
    }

    pub fn is_command(&self) -> bool {
        matches!(self, Self::Command(_))
    }
}

struct OpenRequest {
    first_ms: i64,
    last_ms: i64,
    subject: Option<Subject>,
    queries: PerRequest,
    calls: PerRequest,
    messages: PerRequest,
    cache: PerRequest,
}

pub struct FinishedRequest {
    pub subject: Subject,
    pub ms: f64,
    pub queries: Vec<(u64, u32)>,
    pub query_count: u32,
    pub calls: Vec<(u64, u32)>,
    pub call_count: u32,
    pub messages: Vec<(u64, u32)>,
    pub cache: Vec<(u64, u32)>,
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
    /// The token the line carried, kept so that a run can claim its own lines
    /// later. A console command is named only by the line that **ends** it —
    /// Symfony writes nothing when one starts — so everything it logged
    /// before that was recorded belonging to nothing.
    pub token: Option<String>,
}

/// An N+1 pattern: the same SQL query repeated within a single run.
#[derive(Clone)]
pub struct NPlusOne {
    /// What repeated it — an endpoint, or a console command. A cron job is
    /// entitled to an N+1 like any route, and more likely to keep one: a
    /// profiler gets opened on a route, never on a nightly import.
    pub subject: String,
    pub sql: String,
    /// Runs — requests or command runs — where the pattern was observed.
    pub requests: u64,
    /// Worst repetition seen on a single HTTP request.
    pub max_count: u32,
    total_count: u64,
    pub last_seen: Option<DateTime<FixedOffset>>,
}

impl NPlusOne {
    /// Repetitions per run, on average.
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

    /// Records the line in its run and returns the subject known for it —
    /// which is what attributes an error carrying no route context, and what
    /// puts a cron job's queries under the command that ran them.
    fn observe(
        &mut self,
        entry: &LogEntry,
        subject: Option<&Subject>,
        ms: i64,
        shapes: Shapes,
    ) -> Option<Subject> {
        let token = self.token_of(entry)?.to_string();

        if self.open.len() >= MAX_OPEN_REQUESTS && !self.open.contains_key(&token) {
            self.saturated = true;
            return None;
        }
        let open = self.open.entry(token).or_insert_with(|| OpenRequest {
            first_ms: ms,
            last_ms: ms,
            subject: None,
            queries: PerRequest::default(),
            calls: PerRequest::default(),
            messages: PerRequest::default(),
            cache: PerRequest::default(),
        });
        open.last_ms = open.last_ms.max(ms);
        open.first_ms = open.first_ms.min(ms);
        // Compared before cloning: every line of a run names the same
        // subject, and a `String` per line is the hot path's whole budget.
        if let Some(subject) = subject
            && open.subject.as_ref() != Some(subject)
        {
            open.subject = Some(subject.clone());
        }
        for (fingerprint, counter) in [
            (shapes.sql, &mut open.queries),
            (shapes.call, &mut open.calls),
            (shapes.message, &mut open.messages),
            (shapes.cache, &mut open.cache),
        ] {
            if let Some(fingerprint) = fingerprint {
                counter.count(fingerprint);
            }
        }
        open.subject.clone()
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
            if let Some(subject) = &open.subject {
                done.push(FinishedRequest {
                    subject: subject.clone(),
                    ms: (open.last_ms - open.first_ms) as f64,
                    query_count: open.queries.total,
                    queries: open.queries.take(),
                    call_count: open.calls.total,
                    calls: open.calls.take(),
                    messages: open.messages.take(),
                    cache: open.cache.take(),
                });
            }
            false
        });
        done
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// The token this line carries, once a key has been settled on. Read
    /// without settling one: by the time this is asked, `observe` has already
    /// locked the key in.
    fn token_for<'a>(&self, entry: &'a LogEntry) -> Option<&'a str> {
        let key = self.key.as_ref()?;
        entry.lookup(key).and_then(value_as_token)
    }

    /// When the process behind this token wrote its first line.
    ///
    /// A console command logs nothing when it starts, so the only thing that
    /// dates its beginning is the first line it wrote — which this table is
    /// already holding, under the token the whole process shares.
    fn started_at(&mut self, entry: &LogEntry) -> Option<i64> {
        let token = self.token_of(entry)?;
        self.open.get(token).map(|open| open.first_ms)
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
    /// Durations actually measured, whatever their source and whatever the
    /// endpoint — including the lines whose route could not be named, and
    /// those the `MAX_ROUTES` ceiling kept out of the table. It is what says
    /// how much of the read a quantile covers, so like every denominator it
    /// counts everything.
    pub timed: u64,
    /// Error lines raised outside any HTTP request — a console command's,
    /// today. It is a count and not a table: no ceiling applies, since it is
    /// subtracted from a numerator.
    pub errors_off_request: u64,
    pub channels: HashMap<String, ChannelStat>,
    pub errors: HashMap<String, ErrorStat>,
    /// Deprecations, indexed by (normalised message, normalised origin).
    pub deprecations: HashMap<(String, String), DeprecationStat>,
    /// Deprecation lines seen, including those the ceiling kept from being
    /// detailed: a ceiling stops detailing, never counting.
    pub deprecations_total: u64,
    pub routes: HashMap<String, RouteStat>,
    /// N+1 patterns, indexed by (endpoint, SQL query fingerprint).
    pub nplus1: HashMap<(String, u64), NPlusOne>,
    /// Fingerprint → SQL text dictionary: the text is stored once only, and
    /// not inside each of the open requests.
    sql_texts: HashMap<u64, String>,
    /// Console commands, by name fingerprint.
    pub commands: HashMap<u64, CommandStat>,
    /// Console lines read that named a command, including those the ceiling
    /// kept from being detailed.
    pub command_lines: u64,
    /// Reused between lines to fold a cache key without allocating one.
    key_scratch: String,
    /// Cache misses, by folded-key fingerprint.
    pub cache: HashMap<u64, CacheStat>,
    /// Misses read, including those the ceiling kept from being detailed.
    pub cache_misses: u64,
    /// Messages on the Messenger bus, by class fingerprint.
    pub messages: HashMap<u64, MessageStat>,
    /// Messenger lines read, whatever they said: what tells "nothing was
    /// dispatched" from "this log carries no bus at all".
    pub messenger_lines: u64,
    /// Dispatched messages waiting for the line that handles them, by
    /// identifier. Only the **lag** rests on this table, and therefore only
    /// the lag stops at its ceiling.
    open_messages: HashMap<String, (u64, i64)>,
    /// Outbound HTTP calls, by shape fingerprint.
    pub http: HashMap<u64, HttpStat>,
    /// Calls read, including those the ceiling kept from being detailed: a
    /// ceiling stops detailing, never counting.
    pub http_calls: u64,
    /// Durations read on them — what the quantiles of a shape rest on.
    pub http_timed: u64,
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
            errors_off_request: 0,
            timed: 0,
            channels: HashMap::new(),
            errors: HashMap::new(),
            deprecations: HashMap::new(),
            deprecations_total: 0,
            routes: HashMap::new(),
            nplus1: HashMap::new(),
            sql_texts: HashMap::new(),
            commands: HashMap::new(),
            command_lines: 0,
            key_scratch: String::new(),
            cache: HashMap::new(),
            cache_misses: 0,
            messages: HashMap::new(),
            messenger_lines: 0,
            open_messages: HashMap::new(),
            http: HashMap::new(),
            http_calls: 0,
            http_timed: 0,
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
        // Symfony's HttpClient logs every outbound call with its duration and
        // its status. They were volume and nothing else, on calls that cost
        // ten to a hundred times what an SQL query does.
        let call = entry
            .http_call()
            .map(|call| self.record_http_call(&call, entry.ts));
        // Symfony Messenger writes a line for every message handed to a
        // transport and every message a worker takes off one. The gap between
        // the two counts is the consumer that died on Friday evening, and
        // nothing in a log says it more plainly.
        let message = self.record_messenger(&entry, now_ms);
        // Symfony's cache writes a line when it computes an item and nothing
        // when it serves one: every one of these is a miss, and a key that
        // turns up on every request is a cache that is not working.
        let cache = self.record_cache_miss(&entry);

        // What this line belongs to. A console line names its command, every
        // other one its endpoint — and the tracker carries whichever it is,
        // so that a cron job's queries are counted under the command that ran
        // them rather than dropped for naming no route.
        let command = entry.command();
        let own_subject = command
            .as_ref()
            .map(|command| Subject::Command(command.name.clone()))
            .or_else(|| entry.endpoint().map(Subject::Endpoint));
        let known_subject = self.tracker.observe(
            &entry,
            own_subject.as_ref(),
            now_ms,
            Shapes {
                sql,
                call,
                message,
                cache,
            },
        );
        let subject = own_subject.or(known_subject);
        // Borrowed, not copied: everything below is counted over HTTP
        // requests — and a command is not one — but reading that out of the
        // subject must not cost a `String` on every line read.
        let endpoint = match &subject {
            Some(Subject::Endpoint(name)) => Some(name.as_str()),
            _ => None,
        };

        // Recorded after the tracker has seen the line, so the run's first
        // line is already dated: that is the only thing marking where a
        // command began, since Symfony writes nothing when one starts.
        if let Some(command) = command {
            self.record_command(&entry, command, now_ms);
        }

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

        // Counted here and not inside the block below: a duration on a line
        // that names no endpoint is still a duration read, and saying so is
        // the whole point of the coverage.
        if field_ms.is_some() {
            self.timed += 1;
        }

        let status = entry.status();
        if let Some(code) = status {
            self.by_status[(code / 100 - 1) as usize] += 1;
        }

        if let Some(name) = endpoint
            && (counts_as_request || is_error || field_ms.is_some() || status.is_some())
        {
            if self.routes.len() < MAX_ROUTES || self.routes.contains_key(name) {
                let route = self.routes.entry(name.to_string()).or_default();
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
        // Attributed to the subject and not to the endpoint: an exception
        // thrown by a cron job says which command threw it, which is the
        // whole point of following one. Named only where one is needed —
        // errors and deprecations are rare, and the stream takes the name by
        // value at the end rather than a copy of it.
        if is_error {
            if is_off_request(&entry) {
                self.errors_off_request += 1;
            }
            let name = subject.as_ref().map(|s| s.name().to_string());
            self.record_error(&entry, name);
        }

        // -- deprecations --------------------------------------------------
        // Logged at INFO on the `php` channel: without this, they are volume
        // and nothing else — while a log is the one place they all show up
        // before an upgrade, where the profiler shows them one request at a
        // time.
        if entry.is_deprecation() {
            let name = subject.as_ref().map(|s| s.name().to_string());
            self.record_deprecation(&entry, name);
        }

        // -- stream --------------------------------------------------------
        if self.recent.len() >= self.scrollback {
            self.recent.pop_front();
        }
        // Kept only where the subject is still unknown. A line that already
        // knows what it belongs to will never need to claim it later, and a
        // `String` per line costs about eight per cent of the throughput on
        // a corpus where most lines sit inside a request that named itself.
        let subject_name = subject.map(|subject| match subject {
            Subject::Endpoint(name) | Subject::Command(name) => name,
        });
        let token = subject_name
            .is_none()
            .then(|| self.tracker.token_for(&entry).map(str::to_string))
            .flatten();
        self.recent.push_back(StreamEntry {
            entry,
            endpoint: subject_name,
            token,
        });

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
            method: None,
            subjects: Subjects::default(),
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
            stat.subjects.insert(&endpoint);
            stat.endpoint = Some(endpoint);
            stat.method = entry.method().map(str::to_string);
        }
    }

    fn record_deprecation(&mut self, entry: &LogEntry, endpoint: Option<String>) {
        self.deprecations_total += 1;
        let key = entry.deprecation_key();
        if self.deprecations.len() >= MAX_DEPRECATIONS && !self.deprecations.contains_key(&key) {
            self.capped.deprecations = true;
            return;
        }
        let stat = self
            .deprecations
            .entry(key)
            .or_insert_with(|| DeprecationStat {
                count: 0,
                channel: entry.channel.clone(),
                message: String::new(),
                origin: None,
                endpoint: None,
                subjects: Subjects::default(),
                first_seen: entry.ts,
                last_seen: entry.ts,
            });
        stat.count += 1;
        stat.last_seen = entry.ts.or(stat.last_seen);
        stat.message = entry.message.lines().next().unwrap_or_default().to_string();
        if let Some(origin) = entry.exception_origin() {
            stat.origin = Some(origin.to_string());
        }
        if let Some(endpoint) = endpoint {
            stat.subjects.insert(&endpoint);
            stat.endpoint = Some(endpoint);
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
            let subject = finished.subject.name().to_string();
            // An N+1 is the same query repeated within a single run. Since
            // Doctrine logs *prepared* statements (`WHERE id = ?`), two
            // executions of one pattern produce exactly the same string:
            // equality is enough, there is no normalisation to write. A cron
            // job is as entitled to one as a route — more, since nobody ever
            // opens a profiler on it.
            if threshold > 0 {
                for (fingerprint, count) in &finished.queries {
                    if *count >= threshold {
                        self.record_nplus1(&subject, *fingerprint, *count, seen_at);
                    }
                }
            }

            // Before the route ceiling: an outbound call counts wherever the
            // endpoint table has room for it or not. A shape the shape ceiling
            // turned away is simply not there to be updated — its calls were
            // counted as they were read.
            for (fingerprint, count) in &finished.calls {
                if let Some(shape) = self.http.get_mut(fingerprint) {
                    shape.record_request(*count, &subject);
                }
            }
            for (fingerprint, count) in &finished.messages {
                if let Some(class) = self.messages.get_mut(fingerprint) {
                    class.record_request(*count, &subject);
                }
            }
            for (fingerprint, count) in &finished.cache {
                if let Some(key) = self.cache.get_mut(fingerprint) {
                    key.record_request(*count, &subject);
                }
            }

            // Everything below is counted over HTTP requests. A command owes
            // none of it: `timed` is the denominator of "5 of 225,245
            // requests timed", the endpoint table is the one thing #101 kept
            // commands out of, and a command's duration is already read off
            // the line that says it exited — more exact than the gap between
            // its first and last log line.
            if finished.subject.is_command() {
                self.record_command_totals(&subject, &finished);
                continue;
            }

            if !field_mode {
                self.timed += 1;
            }

            let is_new = !self.routes.contains_key(&subject);
            if is_new && self.routes.len() >= MAX_ROUTES {
                self.capped.routes = true;
                continue;
            }
            let route = self.routes.entry(subject).or_default();
            route.add_request_totals(finished.query_count, finished.call_count);
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

    /// What a command run did, now that its lines have been swept up: the
    /// queries and the outbound calls it made. Not its duration, which its
    /// exit line already gave exactly.
    fn record_command_totals(&mut self, name: &str, finished: &FinishedRequest) {
        let Some(command) = self.commands.values_mut().find(|c| c.name == name) else {
            return;
        };
        command.closed_runs += 1;
        command.queries_total += u64::from(finished.query_count);
        command.queries_max = command.queries_max.max(finished.query_count);
        command.calls_total += u64::from(finished.call_count);
        command.calls_max = command.calls_max.max(finished.call_count);
    }

    fn record_nplus1(
        &mut self,
        subject: &str,
        fingerprint: u64,
        count: u32,
        seen_at: Option<DateTime<FixedOffset>>,
    ) {
        let key = (subject.to_string(), fingerprint);
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
            subject: subject.to_string(),
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

    /// Records one console line that names a command.
    fn record_command(&mut self, entry: &LogEntry, line: CommandLine, now_ms: i64) {
        self.command_lines += 1;
        let key = fingerprint(&line.name);

        // Read before the ceiling check: it borrows the tracker, and a
        // command the ceiling turned away must not leave the borrow behind.
        let started = line
            .code
            .is_some()
            .then(|| self.tracker.started_at(entry))
            .flatten();

        if self.commands.len() >= MAX_COMMANDS && !self.commands.contains_key(&key) {
            self.capped.commands = true;
            return;
        }
        let stat = self.commands.entry(key).or_insert_with(|| CommandStat {
            name: line.name.clone(),
            first_seen: entry.ts,
            ..CommandStat::default()
        });
        stat.last_seen = entry.ts.or(stat.last_seen);
        if line.threw {
            stat.threw += 1;
        }
        let ends_the_run = line.code.is_some();
        if let Some(code) = line.code {
            stat.runs += 1;
            stat.last_code = Some(code);
            if code != 0 {
                stat.failed += 1;
            }
            if let Some(first_ms) = started {
                // Zero when the exit line is the only one the run wrote: a
                // command that logs nothing of its own cannot be timed, and
                // recording a zero would drag its quantiles to nothing.
                let ms = (now_ms - first_ms).max(0);
                if ms > 0 {
                    stat.add_duration(ms as f64);
                }
            }
        }

        // The run has just said what it was called. Everything it logged
        // before this line was recorded belonging to nothing, so it claims
        // its own lines now — without which following a cron job would empty
        // the Errors and Stream tabs rather than narrow them.
        if ends_the_run && let Some(token) = self.tracker.token_for(entry).map(str::to_string) {
            self.attribute_run(&token, &line.name);
        }
    }

    /// Puts the lines a run logged under the name it turned out to have.
    ///
    /// Bounded by the stream itself: a command that logged more lines than
    /// `--scrollback` keeps has lost its earliest ones, which is the same
    /// bound everything else in the stream lives under.
    fn attribute_run(&mut self, token: &str, name: &str) {
        let mut errors = Vec::new();
        let mut deprecations = Vec::new();
        for item in self.recent.iter_mut() {
            if item.token.as_deref() != Some(token) || item.endpoint.is_some() {
                continue;
            }
            item.endpoint = Some(name.to_string());
            if item.entry.level.is_error() {
                errors.push(item.entry.signature());
            }
            if item.entry.is_deprecation() {
                deprecations.push(item.entry.deprecation_key());
            }
        }
        for signature in errors {
            if let Some(error) = self.errors.get_mut(&signature) {
                error.subjects.insert(name);
                error.endpoint.get_or_insert_with(|| name.to_string());
            }
        }
        for key in deprecations {
            if let Some(stat) = self.deprecations.get_mut(&key) {
                stat.subjects.insert(name);
                stat.endpoint.get_or_insert_with(|| name.to_string());
            }
        }
    }

    /// Records one cache miss and returns its key fingerprint, so the open
    /// request can count how many times it missed on the same item.
    fn record_cache_miss(&mut self, entry: &LogEntry) -> Option<u64> {
        let line = entry.cache_miss()?;
        self.cache_misses += 1;
        // Folded like an error signature: `product_42_teasers` and
        // `product_1337_teasers` are one cache entry family, not two. Folded
        // into a buffer that outlives the call, since the row it names is
        // almost always there already and the text would be thrown away.
        let mut key = std::mem::take(&mut self.key_scratch);
        crate::parser::normalize_key_into(&line.key, &mut key);
        let fingerprint = fingerprint(&key);

        if self.cache.len() >= MAX_CACHE_KEYS && !self.cache.contains_key(&fingerprint) {
            self.capped.cache_keys = true;
            self.key_scratch = key;
            return None;
        }
        let stat = self.cache.entry(fingerprint).or_insert_with(|| CacheStat {
            key: key.clone(),
            ..CacheStat::default()
        });
        match line.event {
            CacheEvent::Computed => stat.computed += 1,
            CacheEvent::Contended => stat.contended += 1,
        }
        stat.last_seen = entry.ts.or(stat.last_seen);
        // Handed back with its capacity, for the next line.
        self.key_scratch = key;
        Some(fingerprint)
    }

    /// Records one Messenger line and returns the class fingerprint when the
    /// line is a **dispatch**, so the open request can count how many it made.
    ///
    /// Only a dispatch: a message is handled by a worker, outside any HTTP
    /// request, and counting that against the request whose lines happen to
    /// surround it would attribute somebody else's work to it.
    fn record_messenger(&mut self, entry: &LogEntry, now_ms: i64) -> Option<u64> {
        let line = entry.messenger()?;
        self.messenger_lines += 1;
        let key = fingerprint(&line.class);

        // Paired before the ceiling check: a class the ceiling turned away
        // still owes its identifier a removal, or the open table would keep
        // it for ever.
        let lag = self.pair_message(&line, key, now_ms);

        if self.messages.len() >= MAX_MESSAGE_CLASSES && !self.messages.contains_key(&key) {
            self.capped.message_classes = true;
            return None;
        }
        let stat = self.messages.entry(key).or_insert_with(|| MessageStat {
            class: line.class.clone(),
            ..MessageStat::default()
        });
        match (line.event, line.audited) {
            (MessageEvent::Dispatched, false) => stat.dispatched_core += 1,
            (MessageEvent::Dispatched, true) => stat.dispatched_audit += 1,
            (MessageEvent::Handled, false) => stat.handled_core += 1,
            (MessageEvent::Handled, true) => stat.handled_audit += 1,
            _ => {}
        }
        match line.event {
            MessageEvent::Ran => stat.runs += 1,
            MessageEvent::NoHandler => stat.no_handler += 1,
            MessageEvent::Retried => stat.retried += 1,
            MessageEvent::Failed => stat.failed += 1,
            MessageEvent::Dispatched | MessageEvent::Handled => {}
        }
        stat.last_seen = entry.ts.or(stat.last_seen);
        if let Some(ms) = lag {
            stat.add_lag(ms);
        }
        (line.event == MessageEvent::Dispatched).then_some(key)
    }

    /// Pairs a dispatch with its handling through the identifier, and returns
    /// the lag between them.
    ///
    /// `None` for every line that carries no identifier — which is every
    /// dispatch core Symfony writes. The counts do not depend on this; only
    /// the lag does.
    fn pair_message(&mut self, line: &MessengerLine, key: u64, now_ms: i64) -> Option<f64> {
        let id = line.id.as_ref()?;
        match line.event {
            MessageEvent::Dispatched => {
                if self.open_messages.len() >= MAX_OPEN_MESSAGES
                    && !self.open_messages.contains_key(id)
                {
                    self.capped.open_messages = true;
                    return None;
                }
                self.open_messages.insert(id.clone(), (key, now_ms));
                None
            }
            MessageEvent::Handled => {
                let (dispatched_key, at) = self.open_messages.remove(id)?;
                // The same identifier under another class is a transport
                // reusing its tags, not a message that changed shape.
                (dispatched_key == key).then(|| (now_ms - at).max(0) as f64)
            }
            _ => None,
        }
    }

    /// Records one outbound call and returns its shape's fingerprint, so the
    /// open request can count how many of them it made.
    fn record_http_call(&mut self, call: &HttpCall, ts: Option<DateTime<FixedOffset>>) -> u64 {
        let key = fingerprint_of([call.method.as_deref().unwrap_or(""), &call.target]);
        let ms = call.seconds.map(|seconds| seconds * 1000.0);

        // Counted before the ceiling: these two are denominators, and a
        // denominator stops at no ceiling.
        self.http_calls += 1;
        if ms.is_some() {
            self.http_timed += 1;
        }

        if self.http.len() >= MAX_HTTP_SHAPES && !self.http.contains_key(&key) {
            self.capped.http_shapes = true;
            return key;
        }
        let stat = self.http.entry(key).or_insert_with(|| HttpStat {
            // Built here and nowhere else: once per shape, not once per call.
            shape: call.shape(),
            ..HttpStat::default()
        });
        stat.calls += 1;
        stat.last_seen = ts.or(stat.last_seen);
        if let Some(ms) = ms {
            stat.add_duration(ms);
        }
        if let Some(code) = call.status {
            stat.record_status(code);
        }
        key
    }

    /// 64-bit fingerprint of an SQL query, whose text is memorised on the way.
    fn intern_sql(&mut self, sql: &str) -> u64 {
        let fingerprint = fingerprint(sql);

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

    /// Command runs that ended, all commands together.
    pub fn command_runs(&self) -> u64 {
        self.commands.values().map(|c| c.runs).sum()
    }

    /// Those that ended with a non-zero exit code.
    pub fn commands_failed(&self) -> u64 {
        self.commands.values().map(|c| c.failed).sum()
    }

    /// Number of distinct outbound call shapes met.
    pub fn http_shapes(&self) -> usize {
        self.http.len()
    }

    /// Messages dispatched and handled, all classes together: the two figures
    /// whose gap is the backlog.
    pub fn messages_dispatched(&self) -> u64 {
        self.messages.values().map(MessageStat::dispatched).sum()
    }

    pub fn messages_handled(&self) -> u64 {
        self.messages.values().map(MessageStat::handled).sum()
    }

    /// Dispatched with no line saying they were handled, over the window read.
    pub fn messages_waiting(&self) -> u64 {
        self.messages_dispatched()
            .saturating_sub(self.messages_handled())
    }

    pub fn messages_failed(&self) -> u64 {
        self.messages.values().map(|stat| stat.failed).sum()
    }

    /// Error lines a request could have raised: all of them, less what a
    /// subject that is not a request wrote (see `OFF_REQUEST_CHANNELS`).
    pub fn request_errors(&self) -> u64 {
        self.errors_total().saturating_sub(self.errors_off_request)
    }

    /// Those errors divided by HTTP requests — the definition already used per
    /// endpoint, and the only one that does not move with the number of files
    /// handed over to read.
    ///
    /// It counts error **lines**: a request logging an ERROR and a CRITICAL
    /// for the same exception weighs two, so the figure can pass 100 %. That
    /// is a fact about the log, not a defect to hide — while an error no
    /// request could have raised is one, and is what this leaves out.
    ///
    /// `None` when no request was seen: returning 0 would make the threshold
    /// look respected when we have nothing to say about it, exactly like a
    /// quantile on an endpoint that never appeared.
    pub fn request_error_rate(&self) -> Option<f64> {
        match self.requests {
            0 => None,
            requests => Some(self.request_errors() as f64 / requests as f64),
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

/// 64-bit fingerprint of a key coming from the logs: what lets an open request
/// count shapes without carrying their text.
fn fingerprint(text: &str) -> u64 {
    fingerprint_of([text])
}

/// The same, over a key made of several pieces — so that a shape written as
/// `GET host/path` can be fingerprinted without being concatenated first. It
/// is read on every line of its kind and stored once, and building it to hash
/// it was an allocation per line for nothing.
fn fingerprint_of<'a>(parts: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

/// The marker Symfony writes exactly once per HTTP request.
fn is_matched_route(entry: &LogEntry) -> bool {
    entry.channel == "request" && entry.message.starts_with("Matched route")
}

/// Whether an error line belongs to a subject that is not an HTTP request.
///
/// Channel alone, because that is all a line says without a correlation
/// identifier: Symfony's `ConsoleErrorListener` writes on `console`, and no
/// request ever does.
fn is_off_request(entry: &LogEntry) -> bool {
    OFF_REQUEST_CHANNELS
        .iter()
        .any(|channel| entry.channel.eq_ignore_ascii_case(channel))
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

/// The window a report covers: a date on the first bound, and one on the
/// second only when the window crosses a day.
///
/// A time alone lies as soon as the read is longer than a day — and a log read
/// at ten past midnight already covers two of them. Naming the date twice when
/// it is the same one would only add noise.
pub fn format_window(
    first: Option<DateTime<FixedOffset>>,
    last: Option<DateTime<FixedOffset>>,
) -> String {
    match (first, last) {
        (Some(first), Some(last)) => {
            let first = first.with_timezone(&Local);
            let last = last.with_timezone(&Local);
            if first.date_naive() == last.date_naive() {
                format!(
                    "{} → {}",
                    first.format("%Y-%m-%d %H:%M:%S"),
                    last.format("%H:%M:%S")
                )
            } else {
                format!(
                    "{} → {}",
                    first.format("%Y-%m-%d %H:%M:%S"),
                    last.format("%Y-%m-%d %H:%M:%S")
                )
            }
        }
        _ => format!("{} → {}", format_time(first), format_time(last)),
    }
}

/// How much of the read a figure covers: "5 of 225,245 requests".
///
/// `None` when the part is not a share of the whole — a status can sit on a
/// line that is not a request, and "4,263 of 400" would say nothing to anyone.
fn coverage(part: u64, whole: u64) -> Option<String> {
    (whole > 0 && part <= whole)
        .then(|| format!("{} of {} requests", format_count(part), format_count(whole)))
}

/// A span in the two units that carry it. "724422 s" is eight days and a half,
/// and no reader gets that from the digits.
pub fn format_span(secs: f64) -> String {
    if secs < 1.0 {
        return format!("{secs:.1} s");
    }
    let total = secs.round() as u64;
    let (days, hours, minutes, seconds) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
        total % 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds} s"),
        (0, 0, _) => format!("{minutes} min {seconds} s"),
        (0, _, _) => format!("{hours} h {minutes} min"),
        _ => format!("{days} d {hours} h"),
    }
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
            "period   : {} ({})",
            format_window(stats.first_ts, stats.last_ts),
            format_span(stats.span_secs())
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
        // What the rate rests on. Five responses out of two hundred thousand
        // requests answer "0.00 % 5xx" as confidently as a full read would,
        // and the reader has no way to tell the two apart without this.
        let responses = match coverage(stats.responses(), stats.requests) {
            Some(share) => format!("{share} answered"),
            None => format!("{} responses", format_count(stats.responses())),
        };
        let _ = writeln!(
            out,
            "status   : {} — {:.2} % 5xx ({responses})",
            classes.join(" · "),
            rate * 100.0
        );
    }
    let (peak, _) = stats.timeline.peak();
    let _ = writeln!(out, "peak     : {} lines/s", format_count(peak));
    // Same rule for the durations: announcing the field says where they come
    // from, not how many requests carried one.
    let timed = if matches!(stats.duration, DurationSource::Unknown) {
        String::new()
    } else {
        match coverage(stats.timed, stats.requests) {
            Some(share) => format!(" ({share} timed)"),
            None => format!(" ({} timed)", format_count(stats.timed)),
        }
    };
    let _ = writeln!(out, "durations: {}{timed}", stats.duration.label());
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

    // What fills the file — the first question several gigabytes of log
    // raise, and the one thing the dashboard and the JSON both answered
    // while the report, the mode meant for someone with no terminal open,
    // did not.
    let mut channels: Vec<_> = stats.channels.iter().collect();
    channels.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    if !channels.is_empty() {
        let _ = writeln!(out, "\nChannels");
        for (name, channel) in channels.iter().take(10) {
            let errors = if channel.errors > 0 {
                format!("   {:>9} errors", format_count(channel.errors))
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "  {:<16} {:>12} {:>5.1} %{errors}",
                truncate(name, 16),
                format_count(channel.count),
                ratio(channel.count, stats.total) * 100.0
            );
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

    let mut deprecations: Vec<_> = stats.deprecations.iter().collect();
    deprecations.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    if !deprecations.is_empty() {
        let _ = writeln!(
            out,
            "\nDeprecations ({} lines, {} distinct)",
            format_count(stats.deprecations_total),
            format_count(stats.deprecations.len() as u64)
        );
        for ((signature, _), stat) in deprecations.iter().take(10) {
            let _ = writeln!(out, "  {:>7} × {}", format_count(stat.count), signature);
            let mut where_ = Vec::new();
            if let Some(origin) = &stat.origin {
                where_.push(origin.clone());
            }
            if let Some(endpoint) = &stat.endpoint {
                where_.push(format!("last from {endpoint}"));
            }
            if !where_.is_empty() {
                let _ = writeln!(out, "          {}", where_.join(" — "));
            }
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
        // Two different silences, which used to be told as one: nothing was
        // ever measured, or durations were read on lines naming no endpoint —
        // in which case the header announcing a field was right and this line
        // contradicted it.
        let _ = if stats.timed > 0 {
            writeln!(
                out,
                "\n{} durations read, none on a line naming its endpoint.",
                format_count(stats.timed)
            )
        } else {
            writeln!(
                out,
                "\nNo measurable durations. See 'Measuring durations' in the README."
            )
        };
    }

    let mut patterns: Vec<&NPlusOne> = stats.nplus1.values().collect();
    patterns.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| (&a.subject, &a.sql).cmp(&(&b.subject, &b.sql)))
    });
    if !patterns.is_empty() {
        let _ = writeln!(
            out,
            "\nN+1 patterns (the same SQL query repeated within one run)"
        );
        for pattern in patterns.iter().take(10) {
            let _ = writeln!(
                out,
                "  {:<22} {:>4} × at worst, {:>5.1} × on average over {} runs",
                truncate(&pattern.subject, 22),
                pattern.max_count,
                pattern.avg_count(),
                format_count(pattern.requests)
            );
            let _ = writeln!(out, "      {}", truncate(&pattern.sql, 90));
        }
    }

    // The commands, right after the endpoints they are the cron job's
    // counterpart to — and kept apart from them, because every figure above
    // is defined over HTTP requests and a command is not one.
    let mut commands = sorted_commands(stats);
    if !commands.is_empty() {
        let failing = commands.iter().filter(|c| c.failed > 0).count();
        let _ = writeln!(
            out,
            "\nConsole commands ({} distinct, {failing} failing, {} runs)",
            format_count(commands.len() as u64),
            format_count(stats.command_runs())
        );
        commands.truncate(10);
        for command in commands {
            let duration = match command.timed {
                0 => String::new(),
                _ => format!("  p95={:<10}", format_ms(command.quantiles().p95)),
            };
            // What the run did, now that its lines are attributed to it: the
            // figure that finds a nightly import running four thousand
            // queries, which nobody has ever opened a profiler on.
            // Only what there is to say: a command that calls nobody should
            // not carry a column of zeroes across the report.
            let mut work = String::new();
            if command.avg_queries() > 0.0 {
                let _ = write!(work, "  {:.1} SQL/run", command.avg_queries());
            }
            if command.avg_calls() > 0.0 {
                let _ = write!(work, "  {:.1} HTTP/run", command.avg_calls());
            }
            let _ = writeln!(
                out,
                "  {:<28} {:>6} runs {:>6} failed{duration}{work}  last {} at {}",
                truncate(&command.name, 28),
                format_count(command.runs),
                format_count(command.failed),
                command
                    .last_code
                    .map_or_else(|| "—".to_string(), |code| format!("code {code}")),
                format_time(command.last_seen)
            );
            if command.threw > 0 {
                let _ = writeln!(
                    out,
                    "          {} exception(s) logged while it ran",
                    format_count(command.threw)
                );
            }
        }
    }

    // The cache. Every line here is a miss — Symfony writes nothing when it
    // serves an item — so a key at the top of this list on every request is a
    // cache that is not working.
    let mut keys = sorted_cache_keys(stats);
    if !keys.is_empty() {
        let _ = writeln!(
            out,
            "\nCache misses ({} misses, {} keys)",
            format_count(stats.cache_misses),
            format_count(stats.cache.len() as u64)
        );
        keys.truncate(10);
        for key in keys {
            let _ = writeln!(
                out,
                "  {:>7} × {}",
                format_count(key.misses()),
                truncate(&key.key, 60)
            );
            let mut notes = Vec::new();
            // The line that finds a cache doing nothing: a key computed on
            // nearly every request is not a cache, it is a function call with
            // extra steps.
            if let Some(share) = coverage(key.requests, stats.requests) {
                notes.push(format!("on {share}"));
            }
            if key.contended > 0 {
                notes.push(format!(
                    "{} waited on another process computing it",
                    format_count(key.contended)
                ));
            }
            // Only worth naming an endpoint when one stands out: at one miss
            // per request they all do it, and the row would name whichever
            // the tie-break happened to pick.
            if key.max_per_request > 1 {
                notes.push(match &key.worst_subject {
                    Some(endpoint) => format!(
                        "{} × within one request, from {endpoint}",
                        key.max_per_request
                    ),
                    None => format!("{} × within one request", key.max_per_request),
                });
            }
            if !notes.is_empty() {
                let _ = writeln!(out, "          {}", notes.join(" · "));
            }
        }
    }

    // The bus. Placed before the outbound calls because the question it
    // answers is not "why is this slow" but "is anything running at all".
    let mut classes = sorted_message_classes(stats);
    if !classes.is_empty() {
        let waiting = stats.messages_waiting();
        let _ = writeln!(
            out,
            "\nMessages on the bus ({} dispatched, {} handled, {} classes)",
            format_count(stats.messages_dispatched()),
            format_count(stats.messages_handled()),
            format_count(stats.messages.len() as u64)
        );
        if waiting > 0 {
            // The headline of the whole dimension, and the one figure that
            // needs no correlation at all. "Over this read" and not "for
            // ever": a message dispatched in the file's last second is in
            // flight, not lost.
            let _ = writeln!(
                out,
                "  {} dispatched with no handled line over this read",
                format_count(waiting)
            );
        }
        classes.truncate(10);
        for class in classes {
            let _ = writeln!(
                out,
                "  {:<40} {:>10} sent {:>10} handled {:>10} waiting",
                truncate(class.short_name(), 40),
                format_count(class.dispatched()),
                format_count(class.handled()),
                format_count(class.waiting())
            );
            let mut notes = Vec::new();
            if class.failed > 0 {
                notes.push(format!("{} failed", format_count(class.failed)));
            }
            if class.retried > 0 {
                notes.push(format!("{} retried", format_count(class.retried)));
            }
            if class.no_handler > 0 {
                notes.push(format!(
                    "{} with no handler",
                    format_count(class.no_handler)
                ));
            }
            if class.timed > 0 {
                notes.push(format!("lag p95 {}", format_ms(class.quantiles().p95)));
            }
            if class.max_per_request > 1 {
                notes.push(match &class.worst_subject {
                    Some(endpoint) => format!(
                        "{} × dispatched within one request, from {endpoint}",
                        class.max_per_request
                    ),
                    None => format!("{} × dispatched within one request", class.max_per_request),
                });
            }
            if !notes.is_empty() {
                let _ = writeln!(out, "      {}", notes.join(" · "));
            }
        }
    }

    // The outbound calls, last: they are the other half of the explanation a
    // p95 going wrong asks for, and the half no index will fix.
    let mut shapes = sorted_http_shapes(stats);
    if !shapes.is_empty() {
        let _ = writeln!(
            out,
            "\nOutbound HTTP calls ({} calls, {} timed, {} shapes)",
            format_count(stats.http_calls),
            format_count(stats.http_timed),
            format_count(stats.http_shapes() as u64)
        );
        shapes.truncate(10);
        for (shape, quantiles) in shapes {
            let _ = writeln!(
                out,
                "  {:<44} n={:<6} p50={:<10} p95={:<10} max={}",
                truncate(&shape.shape, 44),
                format_count(shape.calls),
                format_ms(quantiles.p50),
                format_ms(quantiles.p95),
                format_ms(shape.max_ms)
            );
            // The second line only when there is something to say on it: a
            // provider answering 429, or one called several times over inside
            // a single request.
            let mut notes = Vec::new();
            if shape.status_4xx > 0 {
                notes.push(format!("{} × 4xx", format_count(shape.status_4xx)));
            }
            if shape.status_5xx > 0 {
                notes.push(format!("{} × 5xx", format_count(shape.status_5xx)));
            }
            if shape.max_per_request > 1 {
                notes.push(match &shape.worst_subject {
                    Some(endpoint) => format!(
                        "{} × at worst within one request, from {endpoint}",
                        shape.max_per_request
                    ),
                    None => format!("{} × at worst within one request", shape.max_per_request),
                });
            }
            if !notes.is_empty() {
                let _ = writeln!(out, "      {}", notes.join(" · "));
            }
        }
    }
    out
}

/// The commands, most troubled first: what failed, then what ran most. The
/// name breaks the tie, so two reads of one log give the same report.
pub fn sorted_commands(stats: &Stats) -> Vec<&CommandStat> {
    let mut commands: Vec<&CommandStat> = stats.commands.values().collect();
    commands.sort_unstable_by(|a, b| {
        b.failed
            .cmp(&a.failed)
            .then_with(|| b.runs.cmp(&a.runs))
            .then_with(|| a.name.cmp(&b.name))
    });
    commands
}

/// The cache keys, most missed first. The key breaks the tie: two reads of one
/// log owe the same report.
pub fn sorted_cache_keys(stats: &Stats) -> Vec<&CacheStat> {
    let mut keys: Vec<&CacheStat> = stats.cache.values().collect();
    keys.sort_unstable_by(|a, b| b.misses().cmp(&a.misses()).then_with(|| a.key.cmp(&b.key)));
    keys
}

/// The message classes, most dispatched first. The name breaks the tie: two
/// reads of one log owe the same report.
fn sorted_message_classes(stats: &Stats) -> Vec<&MessageStat> {
    let mut classes: Vec<&MessageStat> = stats.messages.values().collect();
    classes.sort_unstable_by(|a, b| {
        b.dispatched()
            .cmp(&a.dispatched())
            .then_with(|| b.waiting().cmp(&a.waiting()))
            .then_with(|| a.class.cmp(&b.class))
    });
    classes
}

/// The call shapes, worst latency first. Volume breaks the tie so that shapes
/// nothing was measured on still come out in a useful order, and the name
/// breaks it last: a hash map does not enumerate twice the same way, and two
/// reads of one file must give the same report.
fn sorted_http_shapes(stats: &Stats) -> Vec<(&HttpStat, Quantiles)> {
    let mut shapes: Vec<(&HttpStat, Quantiles)> = stats
        .http
        .values()
        .map(|shape| (shape, shape.quantiles()))
        .collect();
    shapes.sort_unstable_by(|a, b| {
        b.1.p95
            .total_cmp(&a.1.p95)
            .then_with(|| b.0.calls.cmp(&a.0.calls))
            .then_with(|| a.0.shape.cmp(&b.0.shape))
    });
    shapes
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
                // Bare, and the verb beside it: this is what a collector
                // groups by, and `GET app_checkout` groups by nothing.
                "endpoint": error.endpoint,
                "method": error.method,
                // Every subject that raised it, not just the latest above:
                // that a signature comes from six routes is the interesting
                // fact about it. Capped, and `raised_by_capped` says so.
                "raised_by": error.subjects.names().collect::<Vec<_>>(),
                "raised_by_capped": error.subjects.capped,
                "first_seen": error.first_seen.map(|ts| ts.to_rfc3339()),
                "last_seen": error.last_seen.map(|ts| ts.to_rfc3339()),
                "message": error.message.lines().next().unwrap_or_default(),
            })
        })
        .collect();

    let mut deprecations: Vec<_> = stats.deprecations.iter().collect();
    deprecations.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    keep_top(&mut deprecations, top);
    let deprecations: Vec<Value> = deprecations
        .iter()
        .map(|((signature, _), stat)| {
            json!({
                "signature": signature,
                "count": stat.count,
                "channel": stat.channel,
                "origin": stat.origin,
                "endpoint": stat.endpoint,
                "raised_by": stat.subjects.names().collect::<Vec<_>>(),
                "raised_by_capped": stat.subjects.capped,
                "first_seen": stat.first_seen.map(|ts| ts.to_rfc3339()),
                "last_seen": stat.last_seen.map(|ts| ts.to_rfc3339()),
                "message": stat.message,
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
                "http_calls_avg": round(f64::from(route.avg_calls()), 1),
                "http_calls_max": route.calls_max,
            })
        })
        .collect();

    let mut patterns: Vec<&NPlusOne> = stats.nplus1.values().collect();
    patterns.sort_unstable_by(|a, b| {
        b.max_count
            .cmp(&a.max_count)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| (&a.subject, &a.sql).cmp(&(&b.subject, &b.sql)))
    });
    keep_top(&mut patterns, top);
    let nplus1: Vec<Value> = patterns
        .iter()
        .map(|pattern| {
            json!({
                "subject": pattern.subject,
                "sql": pattern.sql,
                "runs_affected": pattern.requests,
                "max_per_request": pattern.max_count,
                "avg_per_request": round(f64::from(pattern.avg_count()), 1),
                "last_seen": pattern.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let mut commands = sorted_commands(stats);
    keep_top(&mut commands, top);
    let commands: Vec<Value> = commands
        .iter()
        .map(|command| {
            let quantiles = command.quantiles();
            json!({
                "command": command.name,
                "runs": command.runs,
                "failed": command.failed,
                "failure_rate": command.failure_rate().map(|r| round(r, 4)),
                "threw": command.threw,
                "last_code": command.last_code,
                // What its runs did, over the runs whose lines the
                // correlation could tie together.
                "closed_runs": command.closed_runs,
                "queries_avg": round(f64::from(command.avg_queries()), 1),
                "queries_max": command.queries_max,
                "http_calls_avg": round(f64::from(command.avg_calls()), 1),
                "http_calls_max": command.calls_max,
                // Null rather than zero where no line of the run could be
                // tied to it: a command writes nothing when it starts.
                "timed": command.timed,
                "p50_ms": (command.timed > 0).then(|| round(f64::from(quantiles.p50), 2)),
                "p95_ms": (command.timed > 0).then(|| round(f64::from(quantiles.p95), 2)),
                "max_ms": (command.timed > 0).then(|| round(f64::from(command.max_ms), 2)),
                "avg_ms": (command.timed > 0).then(|| round(f64::from(command.avg_ms()), 2)),
                "first_seen": command.first_seen.map(|ts| ts.to_rfc3339()),
                "last_seen": command.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let mut keys = sorted_cache_keys(stats);
    keep_top(&mut keys, top);
    let cache: Vec<Value> = keys
        .iter()
        .map(|key| {
            json!({
                "key": key.key,
                "misses": key.misses(),
                "computed": key.computed,
                "contended": key.contended,
                "requests_affected": key.requests,
                "avg_per_request": round(f64::from(key.avg_per_request()), 1),
                "max_per_request": key.max_per_request,
                "worst_subject": key.worst_subject,
                "last_seen": key.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let mut classes = sorted_message_classes(stats);
    keep_top(&mut classes, top);
    let messages: Vec<Value> = classes
        .iter()
        .map(|class| {
            let quantiles = class.quantiles();
            json!({
                "class": class.class,
                "dispatched": class.dispatched(),
                "handled": class.handled(),
                // Over the window read, not for ever: see `waiting`.
                "waiting": class.waiting(),
                "handler_runs": class.runs,
                "no_handler": class.no_handler,
                "retried": class.retried,
                "failed": class.failed,
                // Null rather than zero when no identifier paired a dispatch
                // with its handling: core Symfony logs none on dispatch.
                "lag_timed": class.timed,
                "lag_p50_ms": (class.timed > 0).then(|| round(f64::from(quantiles.p50), 2)),
                "lag_p95_ms": (class.timed > 0).then(|| round(f64::from(quantiles.p95), 2)),
                "lag_max_ms": (class.timed > 0).then(|| round(f64::from(class.max_ms), 2)),
                "requests_affected": class.requests,
                "avg_per_request": round(f64::from(class.avg_per_request()), 1),
                "max_per_request": class.max_per_request,
                "worst_subject": class.worst_subject,
                "last_seen": class.last_seen.map(|ts| ts.to_rfc3339()),
            })
        })
        .collect();

    let mut shapes = sorted_http_shapes(stats);
    keep_top(&mut shapes, top);
    let http_calls: Vec<Value> = shapes
        .iter()
        .map(|(shape, quantiles)| {
            json!({
                "shape": shape.shape,
                "calls": shape.calls,
                "timed": shape.timed,
                "p50_ms": round(f64::from(quantiles.p50), 2),
                "p95_ms": round(f64::from(quantiles.p95), 2),
                "p99_ms": round(f64::from(quantiles.p99), 2),
                "max_ms": round(f64::from(shape.max_ms), 2),
                "avg_ms": round(f64::from(shape.avg_ms()), 2),
                "responses": shape.responses,
                "status_4xx": shape.status_4xx,
                "status_5xx": shape.status_5xx,
                "requests_affected": shape.requests,
                "avg_per_request": round(f64::from(shape.avg_per_request()), 1),
                "max_per_request": shape.max_per_request,
                "worst_subject": shape.worst_subject,
                "last_seen": shape.last_seen.map(|ts| ts.to_rfc3339()),
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
            // The numerator of the rate below: the error lines left once what
            // no request could have raised is set aside.
            "request_errors": stats.request_errors(),
            "request_error_rate": stats.request_error_rate().map(|r| round(r, 4)),
            "deprecations": stats.deprecations_total,
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
            DurationSource::Unknown => json!({ "kind": "none", "timed": stats.timed }),
            DurationSource::Field { key, .. } => {
                json!({ "kind": "field", "key": key, "timed": stats.timed })
            }
            DurationSource::Correlated { key } => {
                json!({ "kind": "correlation", "key": key, "timed": stats.timed })
            }
        },
        "open_requests": stats.tracker.open_count(),
        // Empty the rest of the time: what it holds is no longer detailed in
        // full, and the matching lists are therefore partial.
        "capped": stats.capped.names(),
        "sql": {
            "shapes": stats.sql_shapes(),
            "nplus1_threshold": stats.nplus1_threshold,
        },
        // Commands are counted apart from requests on purpose: `totals`,
        // `throughput` and `endpoints` above are all over HTTP requests, and
        // a command is not one.
        "console": {
            "lines": stats.command_lines,
            "commands": stats.commands.len(),
            "runs": stats.command_runs(),
            "failed": stats.commands_failed(),
        },
        // Every cache line Symfony writes is a miss: it logs when it computes
        // an item and stays silent when it serves one.
        "cache": {
            "misses": stats.cache_misses,
            "keys": stats.cache.len(),
        },
        // The bus. `waiting` is `dispatched - handled` over the window read:
        // a consumer that stopped shows up here and nowhere else.
        "messenger": {
            "lines": stats.messenger_lines,
            "classes": stats.messages.len(),
            "dispatched": stats.messages_dispatched(),
            "handled": stats.messages_handled(),
            "waiting": stats.messages_waiting(),
            "failed": stats.messages_failed(),
        },
        // Outbound calls: `timed` says how many of them carried a
        // `total_time`, and therefore what the quantiles below rest on.
        "http_client": {
            "calls": stats.http_calls,
            "timed": stats.http_timed,
            "shapes": stats.http_shapes(),
        },
        "channels": channels,
        "errors": errors,
        "deprecations": deprecations,
        "endpoints": endpoints,
        "nplus1": nplus1,
        "http_calls": http_calls,
        "messages": messages,
        "cache_keys": cache,
        "commands": commands,
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

    const TS: &str = "2026-09-09T10:00:00.000000+02:00";
    const TS_LATER: &str = "2026-09-09T10:00:00.500000+02:00";

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
                    r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}","duration_ms":120}} {{"token":"{route}"}}"#
                );
                ingest_line(&mut stats, &line);
                // Every route calls the same provider the same number of
                // times: they all tie, on the row and on the endpoint named
                // beside it.
                stats.ingest(
                    0,
                    outbound_line(&route, "https://api.test/v1/ping", 200, 0.1),
                );
            }
            stats.finalize();
            let doc: Value =
                serde_json::from_str(&render_json(&stats, 0, false)).expect("some JSON");
            (
                render_summary(&stats),
                doc["endpoints"].clone(),
                doc["http_calls"].clone(),
            )
        };

        let (summary, endpoints, calls) = read_once();
        let (again, same_again, same_calls) = read_once();
        assert_eq!(endpoints.as_array().expect("a list").len(), 50);
        assert_eq!(endpoints, same_again, "the JSON order must be reproducible");
        // Fifty routes, one call each: the endpoint named beside the provider
        // is decided by the tie-break alone, and `sweep` closes them in
        // whatever order its hash map hands them over.
        assert_eq!(calls[0]["max_per_request"], 1, "a genuine tie");
        assert_eq!(calls[0]["requests_affected"], 50, "between fifty routes");
        assert_eq!(calls[0]["worst_subject"], "a", "broken by name");
        assert_eq!(calls, same_calls, "including the endpoint a tie names");
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
    fn the_summary_names_the_channels_that_fill_the_log() {
        // Two thirds of a real file were `security.DEBUG` and a sixth
        // deprecations: eighty-four per cent of it was two channels a
        // developer could silence that afternoon, and the summary was the one
        // mode that could not say so.
        let mut stats = stats();
        for _ in 0..7 {
            ingest_line(
                &mut stats,
                r#"[2026-09-09T10:00:00.000000+02:00] security.DEBUG: Checking for guard authentication {} []"#,
            );
        }
        for _ in 0..2 {
            ingest_line(&mut stats, &route_line("app_home"));
        }
        ingest_line(
            &mut stats,
            r#"[2026-09-09T10:00:00.000000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boum: "nope" at /var/www/src/X.php line 12 {} []"#,
        );

        let summary = render_summary(&stats);
        let lines: Vec<&str> = summary
            .lines()
            .skip_while(|l| !l.starts_with("Channels"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .collect();

        assert_eq!(lines.len(), 2, "one line per channel: {summary}");
        assert!(
            lines[0].contains("security") && lines[0].contains("70.0 %"),
            "the fullest channel comes first, with its share: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("request") && lines[1].contains("1 errors"),
            "and a channel says how many of its lines are errors: {:?}",
            lines[1]
        );
    }

    #[test]
    fn a_figure_says_how_many_requests_it_covers() {
        // Five lines in two million carried a status, and the report answered
        // "0.00 % 5xx" with the assurance of a full read. One request in a
        // thousand here, and the summary says which.
        let mut stats = stats();
        for _ in 0..999 {
            ingest_line(&mut stats, &route_line("app_home"));
        }
        ingest_line(
            &mut stats,
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_slow". {"route":"app_slow","status":200,"duration_ms":120} []"#,
        );

        let summary = render_summary(&stats);
        assert_eq!(stats.requests, 1_000);
        assert!(
            summary.contains("(1 of 1,000 requests timed)"),
            "the durations say what they cover: {summary}"
        );
        assert!(
            summary.contains("(1 of 1,000 requests answered)"),
            "so does the status: {summary}"
        );
    }

    #[test]
    fn a_duration_read_off_any_endpoint_is_not_a_missing_duration() {
        // The header announced "field 'duration_ms'" and the footer, thirty
        // lines below, said there were no measurable durations. Both were
        // right: the durations were read on lines naming no endpoint. The
        // report now says that, instead of saying two things at once.
        let mut stats = stats();
        ingest_line(&mut stats, &route_line("app_home"));
        ingest_line(
            &mut stats,
            r#"[2026-09-09T10:00:00.000000+02:00] app.INFO: Job done {"duration_ms":42} []"#,
        );

        assert_eq!(stats.timed, 1, "a duration with no endpoint is still read");
        let summary = render_summary(&stats);
        assert!(
            summary.contains("1 durations read, none on a line naming its endpoint."),
            "{summary}"
        );
        assert!(
            !summary.contains("No measurable durations"),
            "the two silences are no longer told as one: {summary}"
        );
    }

    #[test]
    fn the_period_carries_its_date_when_the_window_crosses_a_day() {
        // "20:56:53 → 06:10:34" read as one evening; the log ran for eight
        // days. The date settles it — and is named twice only when it moves.
        // Built from the local noon so the test holds in any timezone.
        use chrono::TimeZone;
        let noon = Local
            .from_local_datetime(
                &Local::now()
                    .date_naive()
                    .and_hms_opt(12, 0, 0)
                    .expect("a valid noon"),
            )
            .single()
            .expect("an unambiguous noon")
            .fixed_offset();

        let same_day = format_window(Some(noon), Some(noon + chrono::Duration::hours(4)));
        let (first, last) = same_day.split_once(" → ").expect("two bounds");
        assert!(
            first.contains('-'),
            "the first bound dates itself: {same_day}"
        );
        assert!(
            !last.contains('-'),
            "the second does not repeat the same date: {same_day}"
        );

        let eight_days = format_window(Some(noon - chrono::Duration::days(8)), Some(noon));
        let (first, last) = eight_days.split_once(" → ").expect("two bounds");
        assert!(first.contains('-') && last.contains('-'), "{eight_days}");
        assert_ne!(first, last);
    }

    #[test]
    fn a_span_is_written_in_units_and_not_in_raw_seconds() {
        assert_eq!(format_span(0.4), "0.4 s");
        assert_eq!(format_span(42.0), "42 s");
        assert_eq!(format_span(841.0), "14 min 1 s");
        assert_eq!(format_span(7_200.0), "2 h 0 min");
        // The figure that opened the issue: eight days and a half, which
        // "724422 s" never said.
        assert_eq!(format_span(724_422.0), "8 d 9 h");
    }

    #[test]
    fn the_summary_dates_a_window_that_spans_days() {
        let line = |ts: &str| {
            format!(r#"[{ts}] request.INFO: Matched route "app_home". {{"route":"app_home"}} []"#)
        };
        let mut stats = stats();
        ingest_line(&mut stats, &line("2026-09-04T12:00:00.000000+02:00"));
        ingest_line(&mut stats, &line("2026-09-12T12:00:00.000000+02:00"));

        let period = render_summary(&stats)
            .lines()
            .find(|l| l.starts_with("period"))
            .expect("a period line")
            .to_string();
        assert_eq!(
            period.matches("2026-09-").count(),
            2,
            "both bounds are dated: {period}"
        );
        assert!(period.contains(" d "), "the span is in days: {period}");
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
    fn a_console_error_is_reported_but_does_not_weigh_on_the_request_rate() {
        // A nightly command failing in the same file used to push the rate
        // past 100 %: its errors were divided by HTTP requests, which never
        // ran them. Ten requests, one error of their own, two the command
        // raised.
        let request_error = r#"[2026-09-09T10:00:00.000000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boum: "nope" at /var/www/src/X.php line 12 {} []"#;
        let console_error = r#"[2026-09-09T10:00:00.000000+02:00] console.CRITICAL: Error thrown while running command "app:import". Message: "nope" {} []"#;

        let mut stats = stats();
        for _ in 0..10 {
            ingest_line(&mut stats, &route_line("app_home"));
        }
        ingest_line(&mut stats, request_error);
        ingest_line(&mut stats, console_error);
        ingest_line(&mut stats, console_error);

        assert_eq!(stats.errors_total(), 3, "three error lines were read");
        assert_eq!(
            stats.request_errors(),
            1,
            "one of them could come from a request"
        );
        assert_eq!(stats.request_error_rate(), Some(0.1));

        // Set aside from the denominator, not from the report: a failing
        // command is still something to see, and `error-rate` — over all
        // lines, answering "how noisy is this log" — still counts it.
        assert_eq!(stats.errors.len(), 2, "both signatures are listed");
        assert_eq!(stats.channels["console"].errors, 2);
    }

    #[test]
    fn the_request_rate_holds_the_errors_of_a_request_that_logs_twice() {
        // The same exception written at ERROR then at CRITICAL is two lines,
        // and the figure says so: a rate over lines can pass 100 % without
        // being wrong. Hiding that would need a per-request attribution the
        // log does not always carry.
        let error = r#"[2026-09-09T10:00:00.000000+02:00] request.ERROR: Uncaught PHP Exception App\Exception\Boum: "nope" at /var/www/src/X.php line 12 {} []"#;
        let critical = r#"[2026-09-09T10:00:00.000000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boum: "nope" at /var/www/src/X.php line 12 {} []"#;

        let mut stats = stats();
        ingest_line(&mut stats, &route_line("app_home"));
        ingest_line(&mut stats, error);
        ingest_line(&mut stats, critical);

        assert_eq!(stats.request_errors(), 2);
        assert_eq!(stats.request_error_rate(), Some(2.0));
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

    /// A deprecation as Symfony's ErrorHandler writes it, from a given route
    /// and with a given subject — the class name that the normaliser erases.
    fn deprecation_line(route: &str, class: &str) -> String {
        format!(
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}"}} {{"token":"{route}"}}
[2026-09-09T10:00:00.010000+02:00] php.INFO: User Deprecated: Since app 2.0: The "{class}" class is deprecated. {{"exception":"[object] (ErrorException(code: 0): User Deprecated: Since app 2.0: The \"{class}\" class is deprecated. at /var/www/src/{class}.php:12)"}} {{"token":"{route}"}}"#
        )
    }

    fn ingest_lines(stats: &mut Stats, lines: &str) {
        for line in lines.lines() {
            ingest_line(stats, line);
        }
    }

    #[test]
    fn one_deprecation_reached_from_many_routes_is_one_row() {
        let mut stats = stats();
        for route in ["app_home", "app_search", "app_checkout"] {
            ingest_lines(&mut stats, &deprecation_line(route, "Legacy"));
        }
        // Same message, same origin: one row, whatever triggered it — and
        // the route is the latest one, a hint about where to look.
        assert_eq!(stats.deprecations.len(), 1, "one thing to fix, not three");
        let stat = stats.deprecations.values().next().unwrap();
        assert_eq!(stat.count, 3);
        assert_eq!(stat.endpoint.as_deref(), Some("app_checkout"));
        assert_eq!(stat.origin.as_deref(), Some("/var/www/src/Legacy.php:12"));
        assert!(stat.message.starts_with("User Deprecated: Since app 2.0"));
        assert_eq!(stats.deprecations_total, 3);

        // Logged at INFO, a deprecation is not an error: the error counters
        // must not have moved.
        assert_eq!(stats.errors_total(), 0);
        assert!(stats.errors.is_empty());

        // The origin tells two deprecated classes apart, where the normaliser
        // alone — which erases quoted names — would have folded them.
        ingest_lines(&mut stats, &deprecation_line("app_home", "Ancient"));
        assert_eq!(stats.deprecations.len(), 2);

        let summary = render_summary(&stats);
        assert!(
            summary.contains("Deprecations (4 lines, 2 distinct)"),
            "{summary}"
        );
        assert!(summary.contains("last from app_checkout"), "{summary}");
        let json: Value = serde_json::from_str(&render_json(&stats, 25, false)).expect("some JSON");
        assert_eq!(json["totals"]["deprecations"], json!(4));
        assert_eq!(json["deprecations"][0]["count"], json!(3));
        assert_eq!(json["deprecations"][0]["endpoint"], json!("app_checkout"));
        assert_eq!(
            json["deprecations"][0]["origin"],
            json!("/var/www/src/Legacy.php:12")
        );
    }

    #[test]
    fn the_deprecation_ceiling_stops_the_table_without_stopping_the_counters() {
        let mut stats = stats();
        // The class name goes into the origin path, which is what keeps the
        // keys distinct once the message has been normalised.
        for i in 0..MAX_DEPRECATIONS {
            ingest_lines(&mut stats, &deprecation_line("app_home", &distinct_name(i)));
        }
        assert_eq!(stats.deprecations.len(), MAX_DEPRECATIONS);
        assert!(!stats.capped.deprecations, "nothing has been refused yet");

        // One more unknown key does not enter the table…
        ingest_lines(&mut stats, &deprecation_line("app_home", "OneTooMany"));
        assert!(stats.capped.deprecations, "the refusal must show");
        assert!(stats.capped.names().contains(&"deprecations"));
        assert_eq!(stats.deprecations.len(), MAX_DEPRECATIONS);
        // …but stays counted: we stop detailing, we do not stop counting.
        assert_eq!(stats.deprecations_total, MAX_DEPRECATIONS as u64 + 1);

        // And a key already known keeps accumulating.
        ingest_lines(&mut stats, &deprecation_line("app_home", &distinct_name(0)));
        assert_eq!(stats.deprecations_total, MAX_DEPRECATIONS as u64 + 2);
        assert_eq!(
            stats
                .deprecations
                .values()
                .filter(|stat| stat.count == 2)
                .count(),
            1
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

    /// One `http_client` response line, as Symfony's HttpClient writes it —
    /// query string and API key included.
    fn outbound_line(token: &str, url: &str, code: u16, seconds: f64) -> LogEntry {
        let line = format!(
            r#"[2026-09-09T10:00:00.070000+02:00] http_client.INFO: Response: "{code} {url}" {seconds:.6} seconds {{"http_method":"GET","http_code":{code},"total_time":{seconds:.6},"url":"{url}"}} {{"token":"{token}"}}"#
        );
        parse_line(&line).expect("valid http_client line")
    }

    #[test]
    fn outbound_calls_are_grouped_timed_and_counted_per_request() {
        let mut stats = stats();
        // One request calling the same provider four times over: the N+1 on a
        // third party, which costs ten to a hundred times an SQL query.
        let mut entries = request_lines("aaa", true);
        for i in 0..4 {
            entries.insert(
                2,
                outbound_line(
                    "aaa",
                    &format!("https://api.example.com/v1/customers/{i}?key=sk_live_9f3c2a"),
                    200,
                    0.2,
                ),
            );
        }
        // And a second request calling it once.
        entries.extend(request_lines("bbb", true));
        entries.insert(
            entries.len() - 1,
            outbound_line(
                "bbb",
                "https://api.example.com/v1/customers/9?key=sk",
                429,
                1.5,
            ),
        );
        for entry in entries {
            stats.ingest(0, entry);
        }
        stats.finalize();

        // The identifier folds, so the five calls are one shape.
        assert_eq!(stats.http_shapes(), 1, "one provider endpoint, one row");
        assert_eq!(stats.http_calls, 5);
        assert_eq!(stats.http_timed, 5);

        let shape = stats.http.values().next().expect("the shape");
        assert_eq!(shape.shape, "GET api.example.com/v1/customers/#");
        assert_eq!(shape.calls, 5);
        assert_eq!(shape.responses, 5);
        // A provider answering 429 is a story the request's own status — 200 —
        // never tells.
        assert_eq!(shape.status_4xx, 1);
        // 0.2 `total_time` is 200 ms and not 0.2: curl measures in seconds.
        // The histogram bounds its error rather than storing the sample, so
        // the quantile is read to within a few per mille.
        let p50 = shape.quantiles().p50;
        assert!((195.0..=205.0).contains(&p50), "p50 = {p50}");
        // The maximum is kept as read, not through the histogram.
        assert_eq!(shape.max_ms, 1500.0);

        // Per request: two requests called it, one of them four times.
        assert_eq!(shape.requests, 2);
        assert_eq!(shape.max_per_request, 4);
        assert_eq!(shape.avg_per_request(), 2.5);
        assert_eq!(shape.worst_subject.as_deref(), Some("app_home"));

        // And the endpoint carries the average, the way it carries SQL/req.
        let route = stats.routes.get("app_home").expect("the endpoint");
        assert_eq!(route.calls_total, 5);
        assert_eq!(route.calls_max, 4);
        assert_eq!(route.avg_calls(), 2.5);
    }

    #[test]
    fn an_outbound_call_weighs_on_nothing_that_describes_our_own_requests() {
        // The line carries a URL and a status, and neither is ours. Counted as
        // a request URI, the third party would become a row of the endpoint
        // table — carrying its API key — and its 429 would land among the
        // responses we served.
        let mut stats = stats();
        for entry in request_lines("aaa", true) {
            stats.ingest(0, entry);
        }
        let before = (stats.requests, stats.by_status, stats.routes.len());
        stats.ingest(
            0,
            outbound_line(
                "aaa",
                "https://api.example.com/v1/geocode?key=sk_live",
                429,
                0.2,
            ),
        );
        stats.finalize();

        assert_eq!(stats.requests, before.0, "not a request of ours");
        assert_eq!(stats.by_status, before.1, "not a response of ours");
        assert_eq!(stats.routes.len(), before.2, "not an endpoint of ours");
        assert!(
            !stats
                .routes
                .keys()
                .any(|name| name.contains("api.example.com")),
            "the provider must not appear as an endpoint"
        );
        assert_eq!(stats.http_calls, 1, "counted where it belongs");
    }

    #[test]
    fn the_outbound_shape_ceiling_stops_detailing_without_stopping_counting() {
        let mut stats = stats();
        for i in 0..MAX_HTTP_SHAPES {
            stats.ingest(
                0,
                outbound_line(
                    "aaa",
                    &format!("https://api.test/{}", distinct_name(i)),
                    200,
                    0.1,
                ),
            );
        }
        assert_eq!(stats.http_shapes(), MAX_HTTP_SHAPES);
        assert!(!stats.capped.http_shapes);

        stats.ingest(
            0,
            outbound_line("aaa", "https://api.test/one-too-many", 200, 0.1),
        );
        assert_eq!(
            stats.http_shapes(),
            MAX_HTTP_SHAPES,
            "no new shape detailed"
        );
        assert!(stats.capped.http_shapes, "and it says so");
        assert_eq!(
            stats.http_calls,
            MAX_HTTP_SHAPES as u64 + 1,
            "the counters carry on"
        );
        assert_eq!(stats.http_timed, MAX_HTTP_SHAPES as u64 + 1);
        assert!(stats.capped.names().contains(&"outbound calls"));
    }

    #[test]
    fn a_call_with_nothing_but_a_status_is_still_counted() {
        // The bare Symfony setup logs no context at all: no `total_time`, no
        // verb. What is left — the provider, its status, how many times one
        // request called it — is still worth having, and must not read as a
        // provider answering instantly.
        let mut stats = stats();
        let line = r#"[2026-09-09T10:00:00.070000+02:00] http_client.INFO: Response: "503 https://api.example.com/v1/geocode?key=sk_live" [] {"token":"aaa"}"#;
        ingest_line(&mut stats, line);

        assert_eq!(stats.http_calls, 1);
        assert_eq!(stats.http_timed, 0, "nothing was measured");
        let shape = stats.http.values().next().expect("the shape");
        assert_eq!(
            shape.shape, "api.example.com/v1/geocode",
            "no verb invented"
        );
        assert_eq!(shape.status_5xx, 1);
        assert_eq!(shape.timed, 0);
        assert_eq!(shape.max_ms, 0.0);
    }

    /// One run of a command: the line it logs of its own, then the line
    /// Symfony writes when it ends.
    fn command_run(token: &str, name: &str, code: i64, at: &str, ends: &str) -> Vec<LogEntry> {
        [
            format!(r#"[{at}] app.INFO: Starting {name} {{"batch":500}} {{"token":"{token}"}}"#),
            format!(
                r#"[{ends}] console.DEBUG: Command "{name} --env=prod" exited with code "{code}" {{"command":"{name} --env=prod","code":{code}}} {{"token":"{token}"}}"#
            ),
        ]
        .iter()
        .map(|line| parse_line(line).expect("valid console line"))
        .collect()
    }

    #[test]
    fn an_error_thrown_by_a_cron_job_says_which_command_threw_it() {
        // The visible half of attributing a command's lines to it: an
        // exception raised during a run carries no route, and used to be
        // filed under nothing at all.
        let mut stats = stats();
        let lines = [
            format!(r#"[{TS}] app.INFO: Starting app:import {{"batch":5}} {{"token":"cmd"}}"#),
            format!(
                r#"[{TS}] app.CRITICAL: Uncaught PHP Exception RuntimeException: "Connection refused" at /var/www/src/X.php line 12 {{"exception":"[object] (RuntimeException(code: 0): Connection refused at /var/www/src/X.php:12)"}} {{"token":"cmd"}}"#
            ),
            format!(
                r#"[{TS_LATER}] console.DEBUG: Command "app:import" exited with code "1" {{"command":"app:import","code":1}} {{"token":"cmd"}}"#
            ),
        ];
        for line in &lines {
            stats.ingest(0, parse_line(line).expect("valid line"));
        }
        stats.finalize();

        let error = stats.errors.values().next().expect("the exception");
        assert_eq!(error.endpoint.as_deref(), Some("app:import"));
        // And the stream can be narrowed to it, which is what `Enter` does.
        assert!(
            stats
                .recent
                .iter()
                .any(|item| item.endpoint.as_deref() == Some("app:import"))
        );
        // Still not an endpoint, and still not a request error.
        assert!(stats.routes.is_empty());
        assert_eq!(stats.request_error_rate(), None, "no request was seen");
    }

    #[test]
    fn a_cron_jobs_queries_are_counted_without_moving_a_single_request_figure() {
        // The whole point, and the whole risk. A command's lines now feed the
        // N+1 table, the outbound calls, the messages and the cache — and
        // none of `requests`, `timed`, `by_status` or the endpoint table,
        // which docs/reports.md promises are over HTTP requests alone.
        let mut alone = stats();
        for entry in request_lines("aaa", true) {
            alone.ingest(0, entry);
        }
        alone.finalize();
        let (requests, timed, status, routes) = (
            alone.requests,
            alone.timed,
            alone.by_status,
            alone.routes.len(),
        );

        // The same request, and a nightly import loading its rows one at a
        // time beside it — the N+1 nobody watches, because a profiler gets
        // opened on a route and never on a cron.
        let mut stats = stats();
        for entry in request_lines("aaa", true) {
            stats.ingest(0, entry);
        }
        let start =
            format!(r#"[{TS}] app.INFO: Starting app:import {{"batch":500}} {{"token":"cmd"}}"#);
        stats.ingest(0, parse_line(&start).expect("valid line"));
        for _ in 0..30 {
            stats.ingest(
                0,
                sql_line("cmd", "SELECT t0.id FROM customer t0 WHERE t0.id = ?"),
            );
        }
        let exit = format!(
            r#"[{TS_LATER}] console.DEBUG: Command "app:import" exited with code "0" {{"command":"app:import","code":0}} {{"token":"cmd"}}"#
        );
        stats.ingest(0, parse_line(&exit).expect("valid line"));
        stats.finalize();

        // Nothing defined over HTTP requests moved.
        assert_eq!(stats.requests, requests, "not a request");
        assert_eq!(stats.timed, timed, "not a duration read on a request");
        assert_eq!(stats.by_status, status, "not a response");
        assert_eq!(stats.routes.len(), routes, "and above all not an endpoint");
        assert!(!stats.routes.contains_key("app:import"));

        // And the import's N+1 is there, under the command that ran it.
        let pattern = stats
            .nplus1
            .values()
            .find(|p| p.subject == "app:import")
            .expect("the import's N+1");
        assert_eq!(pattern.max_count, 30);

        let command = stats
            .commands
            .values()
            .find(|c| c.name == "app:import")
            .expect("the command");
        assert_eq!(command.runs, 1);
        assert_eq!(command.closed_runs, 1);
        assert_eq!(command.avg_queries(), 30.0);
        // Its duration still comes from the exit line and not from the sweep:
        // one measurement per run, the more exact of the two.
        assert_eq!(command.timed, 1);
        assert_eq!(command.max_ms, 500.0);
    }

    #[test]
    fn a_command_is_counted_apart_from_the_requests_it_is_not() {
        // The decision this dimension turned on: `requests`,
        // `request-error-rate` and the peak are all defined over HTTP
        // requests, and a command is not one. A failing nightly import must
        // not land in the endpoint table, nor trip a bare `p95` threshold.
        let mut stats = stats();
        for entry in request_lines("aaa", true) {
            stats.ingest(0, entry);
        }
        for entry in command_run("cmd", "app:import", 1, TS, TS_LATER) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.requests, 1, "the command is not a request");
        assert_eq!(stats.routes.len(), 1, "nor an endpoint");
        assert!(!stats.routes.contains_key("app:import"));
        assert_eq!(stats.command_runs(), 1);
        assert_eq!(stats.commands_failed(), 1);

        let command = stats.commands.values().next().expect("the command");
        assert_eq!(command.name, "app:import", "without its arguments");
        assert_eq!(command.last_code, Some(1));
        assert_eq!(command.failure_rate(), Some(1.0));
        // Symfony logs nothing when a command starts: its duration is the gap
        // between the first line its process wrote and the one saying it
        // exited — 500 ms here.
        assert_eq!(command.timed, 1);
        assert_eq!(command.max_ms, 500.0);
    }

    #[test]
    fn an_exception_and_the_exit_code_are_two_facts_about_one_run() {
        // A command that throws logs twice, and only one of those lines ends
        // the run: counting both as runs would double every command that
        // failed. It can also throw, catch, and still exit zero — which is
        // why the two counters stay apart.
        let mut stats = stats();
        let threw = format!(
            r#"[{TS}] console.CRITICAL: Error thrown while running command "app:import". Message: "Boom" {{"command":"app:import","message":"Boom"}} {{"token":"cmd"}}"#
        );
        stats.ingest(0, parse_line(&threw).expect("valid line"));
        for entry in command_run("cmd", "app:import", 0, TS, TS_LATER) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let command = stats.commands.values().next().expect("the command");
        assert_eq!(command.runs, 1, "one run, two lines about it");
        assert_eq!(command.threw, 1);
        assert_eq!(command.failed, 0, "it caught it and exited zero");
        assert_eq!(command.failure_rate(), Some(0.0));
    }

    #[test]
    fn a_command_never_timed_says_so_rather_than_reporting_nothing() {
        // Without a token tying its lines together — no `UidProcessor`, or a
        // command that logs nothing of its own — there is no duration to be
        // had. The counts stay exact and the quantiles stay empty.
        let mut stats = stats();
        let line = format!(
            r#"[{TS}] console.DEBUG: Command "app:cache:warm" exited with code "0" {{"command":"app:cache:warm","code":0}} []"#
        );
        stats.ingest(0, parse_line(&line).expect("valid line"));
        stats.finalize();

        let command = stats.commands.values().next().expect("the command");
        assert_eq!(command.runs, 1);
        assert_eq!(command.timed, 0, "nothing dated its start");
        assert_eq!(command.max_ms, 0.0);

        let doc: Value =
            serde_json::from_str(&render_json(&stats, 0, false)).expect("well-formed JSON");
        assert_eq!(doc["console"]["runs"], 1);
        assert_eq!(doc["commands"][0]["command"], "app:cache:warm");
        assert_eq!(doc["commands"][0]["failure_rate"], 0.0);
        // Null and not zero: a command with no duration is not an instant one.
        assert!(doc["commands"][0]["p95_ms"].is_null(), "{doc}");
    }

    #[test]
    fn the_command_ceiling_stops_detailing_without_stopping_counting() {
        let mut stats = stats();
        for i in 0..MAX_COMMANDS {
            let line = format!(
                r#"[{TS}] console.DEBUG: Command "{}" exited with code "0" {{"command":"{}","code":0}} []"#,
                distinct_name(i),
                distinct_name(i)
            );
            stats.ingest(0, parse_line(&line).expect("valid line"));
        }
        assert_eq!(stats.commands.len(), MAX_COMMANDS);
        assert!(!stats.capped.commands);

        let extra = format!(
            r#"[{TS}] console.DEBUG: Command "one_too_many" exited with code "0" {{"command":"one_too_many","code":0}} []"#
        );
        stats.ingest(0, parse_line(&extra).expect("valid line"));
        assert_eq!(stats.commands.len(), MAX_COMMANDS, "no new command");
        assert!(stats.capped.commands, "and it says so");
        assert_eq!(
            stats.command_lines,
            MAX_COMMANDS as u64 + 1,
            "the lines stay counted"
        );
        assert!(stats.capped.names().contains(&"commands"));
    }

    fn cache_line(token: &str, key: &str, contended: bool) -> LogEntry {
        let message = match contended {
            true => format!(r#"Item "{key}" is locked, waiting for it to be released"#),
            false => format!(r#"Lock acquired, now computing item "{key}""#),
        };
        let line =
            format!(r#"[{TS}] cache.INFO: {message} {{"key":"{key}"}} {{"token":"{token}"}}"#);
        parse_line(&line).expect("valid cache line")
    }

    #[test]
    fn a_key_computed_on_every_request_is_a_cache_that_is_not_working() {
        // The finding the dimension exists for, and it is invisible any other
        // way: Symfony writes nothing when it serves an item, so a key at the
        // top of this list is one whose cache never hits.
        let mut stats = stats();
        for i in 0..5 {
            let token = format!("r{i}");
            let mut entries = request_lines(&token, true);
            entries.insert(2, cache_line(&token, "nav_menu", false));
            // And one key that varies per product: folded to one row.
            entries.insert(2, cache_line(&token, &format!("product_{i}_detail"), false));
            for entry in entries {
                stats.ingest(0, entry);
            }
        }
        stats.finalize();

        assert_eq!(stats.requests, 5);
        assert_eq!(stats.cache_misses, 10);
        assert_eq!(stats.cache.len(), 2, "five products, one key family");

        let keys = sorted_cache_keys(&stats);
        let names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(names, ["nav_menu", "product_#_detail"]);
        for key in &keys {
            assert_eq!(key.requests, 5, "missed on every request: {}", key.key);
            assert_eq!(key.max_per_request, 1);
        }

        let summary = render_summary(&stats);
        assert!(summary.contains("on 5 of 5 requests"), "{summary}");
    }

    #[test]
    fn the_same_item_computed_twice_in_one_request_is_named() {
        // The lock exists to stop two processes computing one item at once;
        // one request computing it twice over is the same waste, inside a
        // single process, and only a per-request count finds it.
        let mut stats = stats();
        let mut entries = request_lines("aaa", true);
        for _ in 0..3 {
            entries.insert(2, cache_line("aaa", "nav_menu", false));
        }
        entries.insert(2, cache_line("aaa", "nav_menu", true));
        for entry in entries {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let key = stats.cache.values().next().expect("the key");
        assert_eq!(key.computed, 3);
        assert_eq!(key.contended, 1, "one of them waited on another process");
        assert_eq!(key.misses(), 4);
        assert_eq!(key.max_per_request, 4, "all four inside one request");
        assert_eq!(key.worst_subject.as_deref(), Some("app_home"));

        let doc: Value =
            serde_json::from_str(&render_json(&stats, 0, false)).expect("well-formed JSON");
        assert_eq!(doc["cache"]["misses"], 4);
        assert_eq!(doc["cache"]["keys"], 1);
        assert_eq!(doc["cache_keys"][0]["key"], "nav_menu");
        assert_eq!(doc["cache_keys"][0]["contended"], 1);
        assert_eq!(doc["cache_keys"][0]["max_per_request"], 4);
        assert_eq!(doc["cache_keys"][0]["worst_subject"], "app_home");
    }

    #[test]
    fn the_cache_key_ceiling_stops_detailing_without_stopping_counting() {
        let mut stats = stats();
        for i in 0..MAX_CACHE_KEYS {
            stats.ingest(0, cache_line("aaa", &distinct_name(i), false));
        }
        assert_eq!(stats.cache.len(), MAX_CACHE_KEYS);
        assert!(!stats.capped.cache_keys);

        stats.ingest(0, cache_line("aaa", "one_key_too_many", false));
        assert_eq!(stats.cache.len(), MAX_CACHE_KEYS, "no new key detailed");
        assert!(stats.capped.cache_keys, "and it says so");
        assert_eq!(
            stats.cache_misses,
            MAX_CACHE_KEYS as u64 + 1,
            "the counter carries on"
        );
        assert!(stats.capped.names().contains(&"cache keys"));
    }

    /// One dispatch, written by both vocabularies at once, as an application
    /// running an audit middleware beside Symfony's own logging really writes.
    fn dispatch_lines(token: &str, id: &str, class: &str, at: &str) -> Vec<LogEntry> {
        [
            format!(
                r#"[{at}] messenger_audit.INFO: [{id}] Sent {class} {{"id":"{id}","class":"{class}"}} {{"token":"{token}"}}"#
            ),
            format!(
                r#"[{at}] messenger.INFO: Sending message {class} with async sender using X {{"class":"{class}","alias":"async"}} {{"token":"{token}"}}"#
            ),
        ]
        .iter()
        .map(|line| parse_line(line).expect("valid dispatch line"))
        .collect()
    }

    /// The worker taking it off the queue, some time later — with no request
    /// token, because a worker runs outside any HTTP request.
    fn handled_lines(id: &str, class: &str, at: &str) -> Vec<LogEntry> {
        [
            format!(r#"[{at}] messenger_audit.INFO: [{id}] Received {class} [] []"#),
            format!(
                r#"[{at}] messenger.INFO: Message {class} handled by H {{"class":"{class}","handler":"H"}} []"#
            ),
            format!(
                r#"[{at}] messenger.INFO: {class} was handled successfully (acknowledging to transport). {{"class":"{class}","message_id":"{id}"}} []"#
            ),
        ]
        .iter()
        .map(|line| parse_line(line).expect("valid handling line"))
        .collect()
    }

    #[test]
    fn a_dispatch_written_by_two_vocabularies_at_once_is_one_dispatch() {
        // The log that prompted this dimension carries both: the audit
        // middleware's `[id] Sent …` and Symfony's `Sending message …`, for
        // one and the same message. Adding them would double every figure on
        // the row — and the queue would look twice as deep as it is.
        let mut stats = stats();
        let class = "App_Message_Index";
        for i in 0..3 {
            for entry in dispatch_lines("aaa", &format!("id{i}"), class, TS) {
                stats.ingest(0, entry);
            }
        }
        // One of the three gets handled — also written twice, plus the
        // `handled by` line, which is a handler run and not an acknowledgement.
        for entry in handled_lines("id0", class, TS) {
            stats.ingest(0, entry);
        }
        stats.finalize();

        assert_eq!(stats.messenger_lines, 9, "nine lines read");
        assert_eq!(stats.messages_dispatched(), 3, "but three dispatches");
        assert_eq!(stats.messages_handled(), 1, "and one handled");
        assert_eq!(stats.messages_waiting(), 2);

        let stat = stats.messages.values().next().expect("the class");
        assert_eq!(stat.runs, 1, "the handler ran once");
    }

    #[test]
    fn the_gap_between_dispatched_and_handled_is_the_consumer_that_stopped() {
        // The figure the whole dimension exists for, and the one that needs no
        // correlation at all: a queue that is not draining says so in two
        // counters. 89,338 sent against 374 received is what a real file
        // looked like when nobody had noticed the worker had died.
        let mut stats = stats();
        for i in 0..40 {
            for entry in dispatch_lines("aaa", &format!("m{i}"), "App_Message_Index", TS) {
                stats.ingest(0, entry);
            }
        }
        for i in 0..2 {
            for entry in handled_lines(&format!("m{i}"), "App_Message_Index", TS_LATER) {
                stats.ingest(0, entry);
            }
        }
        stats.finalize();

        assert_eq!(stats.messages_dispatched(), 40);
        assert_eq!(stats.messages_handled(), 2);
        assert_eq!(stats.messages_waiting(), 38);

        // And the lag, which the audit identifier is the only thing that
        // gives: 500 ms between the two timestamps.
        let stat = stats.messages.values().next().expect("the class");
        assert_eq!(stat.timed, 2, "the two that were paired");
        assert_eq!(stat.max_ms, 500.0);
    }

    #[test]
    fn a_dispatch_loop_inside_one_request_is_counted_against_its_endpoint() {
        // The N+1 on the bus, one layer above the SQL one: one message per
        // row of a listing, each of them a job someone will have to run.
        let mut stats = stats();
        let mut entries = request_lines("aaa", true);
        for i in 0..12 {
            entries.insert(
                2,
                dispatch_lines("aaa", &format!("n{i}"), "App_Message_Index", TS)[0].clone(),
            );
        }
        // A second request dispatches one, so the average is not the worst.
        entries.extend(request_lines("bbb", true));
        entries.insert(
            entries.len() - 1,
            dispatch_lines("bbb", "solo", "App_Message_Index", TS)[0].clone(),
        );
        for entry in entries {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let stat = stats.messages.values().next().expect("the class");
        assert_eq!(stat.dispatched(), 13);
        assert_eq!(stat.requests, 2);
        assert_eq!(stat.max_per_request, 12);
        assert_eq!(stat.avg_per_request(), 6.5);
        assert_eq!(stat.worst_subject.as_deref(), Some("app_home"));

        // A worker handles messages outside any HTTP request: what it does
        // must not be charged to whichever request its lines happen to sit
        // beside.
        let route = stats.routes.get("app_home").expect("the endpoint");
        assert_eq!(route.requests, 2);
    }

    #[test]
    fn the_message_class_ceiling_stops_detailing_without_stopping_counting() {
        let mut stats = stats();
        for i in 0..MAX_MESSAGE_CLASSES {
            for entry in dispatch_lines("aaa", "x", &distinct_name(i), TS) {
                stats.ingest(0, entry);
            }
        }
        assert_eq!(stats.messages.len(), MAX_MESSAGE_CLASSES);
        assert!(!stats.capped.message_classes);

        let before = stats.messenger_lines;
        for entry in dispatch_lines("aaa", "y", "one_class_too_many", TS) {
            stats.ingest(0, entry);
        }
        assert_eq!(stats.messages.len(), MAX_MESSAGE_CLASSES, "no new class");
        assert!(stats.capped.message_classes, "and it says so");
        assert_eq!(stats.messenger_lines, before + 2, "the lines stay counted");
        assert!(stats.capped.names().contains(&"message classes"));
    }

    #[test]
    fn no_query_string_reaches_the_summary_or_the_json() {
        // The URL that prompted the whole dimension holds an API key. Nothing
        // refrain derives from it may carry that key out — not the summary a
        // cron job mails, not the JSON a collector stores, not the shape a
        // user copies. Dropped at the parser, checked here at the far end.
        let mut stats = stats();
        let url = "https://api.example.com/v1/geocode?q=12+rue&key=sk_live_9f3c2a";
        let mut entries = request_lines("aaa", true);
        entries.insert(2, outbound_line("aaa", url, 200, 0.2));
        for entry in entries {
            stats.ingest(0, entry);
        }
        stats.finalize();

        let summary = render_summary(&stats);
        let json = render_json(&stats, 0, false);
        for output in [&summary, &json] {
            assert!(
                output.contains("api.example.com/v1/geocode"),
                "the shape is there"
            );
            assert!(!output.contains("sk_live"), "but not the key:\n{output}");
            assert!(
                !output.contains("q=12"),
                "nor the rest of the query:\n{output}"
            );
        }

        let doc: Value = serde_json::from_str(&json).expect("well-formed JSON");
        assert_eq!(doc["http_client"]["calls"], 1);
        assert_eq!(doc["http_client"]["timed"], 1);
        assert_eq!(doc["http_client"]["shapes"], 1);
        let call = &doc["http_calls"][0];
        assert_eq!(call["shape"], "GET api.example.com/v1/geocode");
        assert_eq!(call["calls"], 1);
        assert_eq!(call["max_per_request"], 1);
        assert_eq!(call["worst_subject"], "app_home");
        assert_eq!(doc["endpoints"][0]["http_calls_avg"], 1.0);
        assert_eq!(doc["endpoints"][0]["http_calls_max"], 1);
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
    fn the_peak_does_not_depend_on_which_source_runs_ahead() {
        // Two files of the same half hour, each on its own thread. `prod.log`
        // is short and its reader runs an hour ahead; `doctrine.log`, behind,
        // then delivers its share of a second the ring has already left. That
        // share used to be dropped, and the peak of the same lines read 547 in
        // one file and 334 in two.
        let mut timeline = Timeline::new(600);
        let busy = 1_757_000_000;
        for _ in 0..100 {
            timeline.record(busy, false);
        }
        timeline.record(busy + 3600, false);
        for _ in 0..300 {
            timeline.record(busy, false);
        }
        assert_eq!(timeline.peak(), (400, busy), "the whole busy second");

        // The ring, itself, has moved on: it describes the present.
        assert_eq!(timeline.series(600, |b| b.total).iter().sum::<u64>(), 1);

        // A second before the origin is reached too: the first file handed
        // over is not always the oldest.
        for _ in 0..500 {
            timeline.record(busy - 60, false);
        }
        assert_eq!(timeline.peak(), (500, busy - 60));

        // Beyond the exact span the ring takes over, as before — bounded
        // memory comes first.
        timeline.record(busy + MAX_EXACT_SPAN as i64 + 1, false);
        assert_eq!(timeline.peak(), (500, busy - 60));
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
        assert_eq!(pattern.subject, "app_home");
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
        // How many requests were timed: a consumer reading a quantile needs
        // to know what it rests on.
        assert_eq!(doc["duration_source"]["timed"], doc["totals"]["requests"]);

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
