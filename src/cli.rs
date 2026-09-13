//! Definition of the command-line options.
//!
//! `clap` with the `derive` feature builds the whole argument parser from this
//! structure: the `///` comments become the help shown by `refrain --help`.

use crate::parser::Level;
use crate::threshold::Threshold;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "refrain",
    version,
    about = "Real-time Symfony/Monolog log analyser for your terminal",
    long_about = "Follows Monolog log files, parses them on the fly and shows a \
                  terminal dashboard: errors grouped by type, slowest endpoints, \
                  traffic peaks."
)]
pub struct Cli {
    /// Log files to follow. "-" reads standard input.
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<PathBuf>,

    /// Read the whole file from the start (default: follow from the end).
    #[arg(short = 'a', long)]
    pub from_start: bool,

    /// Re-read the last N lines on start-up, like `tail -n`. With `--summary`
    /// or `--json`, limits the report to that tail of the file.
    #[arg(
        short = 'n',
        long,
        default_value_t = 0,
        value_name = "N",
        conflicts_with = "from_start"
    )]
    pub lines: usize,

    /// Only count entries from this point on: a duration back from start-up
    /// (`30s`, `15m`, `2h`, `3d`), or a date (`2026-09-09T14:30:00`,
    /// `2026-09-09 14:30`, or `14:30` for today).
    ///
    /// Implies reading the file from the start, unless `-n` explicitly caps how
    /// much is re-read — on a forty-gigabyte file, `--since 15m -n 100000`
    /// avoids reading it all to keep a quarter of an hour.
    #[arg(long, value_name = "WHEN", value_parser = parse_bound)]
    pub since: Option<Bound>,

    /// Only count entries up to this point. Same forms as `--since`.
    #[arg(long, value_name = "WHEN", value_parser = parse_bound)]
    pub until: Option<Bound>,

    /// Fail with exit code 3 if a threshold is crossed: `error-rate>2%`,
    /// `p95>1s`, `p95:api_orders_list>800ms`, `entries<100`. Repeatable.
    ///
    /// Metrics: `error-rate`, `request-error-rate`, `5xx-rate`, `errors`,
    /// `entries`, `p50`, `p95`, `p99`, `max`. `error-rate` counts error lines
    /// among all lines, so it drops when a chatty file is added;
    /// `request-error-rate` counts them per HTTP request instead; `5xx-rate`
    /// counts responses, not log levels, and accepts an endpoint;
    /// `deprecations` counts deprecation lines. `http-client-p50` … `-max`
    /// measure the outbound calls, and apply to the worst provider;
    /// `messages-waiting` and `messages-failed` count messages on the bus.
    /// Quantiles apply to the worst endpoint, or to the one named after `:`.
    /// Units: `%`, `ms`, `s`.
    ///
    /// Only for one-shot reports: `--summary`, or `--json` without `--every`.
    #[arg(
        long = "fail-if",
        value_name = "THRESHOLD",
        value_parser = crate::threshold::Threshold::parse,
        conflicts_with = "every"
    )]
    pub fail_if: Vec<Threshold>,

    /// Key in `context`/`extra` holding the duration. Auto-detected if absent.
    #[arg(long, value_name = "KEY")]
    pub duration_key: Option<String>,

    /// Unit of the duration value found.
    #[arg(long, value_enum, default_value_t = DurationUnit::Auto)]
    pub duration_unit: DurationUnit,

    /// Key identifying one request (token, uid, request_id…). Lets refrain
    /// derive endpoint durations when no duration field is logged.
    #[arg(long, value_name = "KEY")]
    pub correlate_key: Option<String>,

    /// Disable correlation even when a key is detected.
    #[arg(long)]
    pub no_correlate: bool,

    /// Idle time (seconds) after which a correlated request is closed.
    #[arg(long, default_value_t = 5.0, value_name = "SEC")]
    pub correlate_timeout: f64,

    /// Minimum level shown in the Stream tab at start (adjust with +/-).
    /// Statistics always count everything, whatever this is set to.
    #[arg(short = 'l', long, value_enum, default_value_t = Level::Debug)]
    pub min_level: Level,

    /// No dashboard: read to the end of the file, then print a text summary.
    /// Handy from cron, from CI, or at the end of an `ssh`.
    #[arg(long)]
    pub summary: bool,

    /// JSON statistics instead of the dashboard, for monitoring. Without
    /// `--every`, reads the files to the end and prints a single object.
    #[arg(long, conflicts_with = "summary")]
    pub json: bool,

    /// With `--json`: keep following and emit one JSON object every SEC
    /// seconds, one per line (NDJSON).
    #[arg(long, value_name = "SEC", requires = "json")]
    pub every: Option<f64>,

    /// N+1 detection threshold: how many times the same SQL query must run
    /// within a single HTTP request to be reported. 0 disables it.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub nplus1: u32,

    /// How many errors, deprecations, endpoints, outbound calls, message
    /// classes and cache keys to detail in JSON. 0 means all of them.
    #[arg(long, default_value_t = 25, value_name = "N")]
    pub top: usize,

    /// Refresh interval of the display, in milliseconds.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    pub tick_ms: u64,

    /// How many entries the Stream tab keeps.
    #[arg(long, default_value_t = 2000, value_name = "N")]
    pub scrollback: usize,
}

/// What the program must produce. Derived from the flags rather than stored:
/// one source of truth, impossible to get out of sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The interactive dashboard (default).
    Tui,
    /// A text summary, once, at the end of the read.
    Summary,
    /// A JSON object, once, at the end of the read.
    JsonOnce,
    /// One JSON object per interval, following continuously (NDJSON).
    JsonStream,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        match (self.json, self.every, self.summary) {
            (true, Some(_), _) => Mode::JsonStream,
            (true, None, _) => Mode::JsonOnce,
            (false, _, true) => Mode::Summary,
            _ => Mode::Tui,
        }
    }

    /// The "one-shot" modes read to the end then hand back; the others stay
    /// attached to the file.
    pub fn follow(&self) -> bool {
        matches!(self.mode(), Mode::Tui | Mode::JsonStream)
    }

    /// A one-shot report covers the whole file… unless `-n` explicitly names
    /// its end: on a forty-gigabyte `prod.log`, "summarise the last hundred
    /// thousand lines for me" is a common request, and ignoring it silently
    /// would reread the whole file.
    pub fn read_from_start(&self) -> bool {
        self.from_start
            || (self.lines == 0 && matches!(self.mode(), Mode::Summary | Mode::JsonOnce))
            // A window starting in the past only makes sense from the start of
            // the file: following from the end would show nothing until a new
            // line arrives.
            || (self.since.is_some() && self.lines == 0)
    }

    /// Period between two NDJSON snapshots, bounded so as not to drown the
    /// output or read the clock in a loop.
    pub fn snapshot_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.every.unwrap_or(10.0).clamp(0.1, 3600.0))
    }
}

/// How to read the numeric value found in the duration field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DurationUnit {
    /// Infer from the key name (`_ms`, `_s`, `_us`), then from magnitude: a
    /// float below 30 is almost always seconds.
    Auto,
    /// Milliseconds.
    Ms,
    /// Seconds.
    S,
    /// Microseconds.
    Us,
}

/// A time bound, as written on the command line.
///
/// It is not resolved here but when the aggregation starts: `--since 15m`
/// designates a fixed instant, taken once and for all, and not a window that
/// would slide under the counters' feet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// A step back, in milliseconds, from start-up.
    Ago(i64),
    /// An instant, in milliseconds since the epoch.
    At(i64),
}

impl Bound {
    pub fn epoch_ms(self, launched_ms: i64) -> i64 {
        match self {
            Bound::Ago(ms) => launched_ms - ms,
            Bound::At(ms) => ms,
        }
    }
}

/// Accepts a relative duration or a date, in the forms one writes without
/// thinking when looking for "since half past two".
fn parse_bound(text: &str) -> Result<Bound, String> {
    let text = text.trim();
    if let Some(ms) = parse_duree(text) {
        return Ok(Bound::Ago(ms));
    }
    if let Some(ms) = parse_instant(text) {
        return Ok(Bound::At(ms));
    }
    Err(format!(
        "'{text}' is neither a duration (30s, 15m, 2h, 3d) nor a date \
         (2026-09-09T14:30:00, '2026-09-09 14:30', 14:30)"
    ))
}

/// `15m` → 900,000 ms. With no unit we refuse: "--since 15" means nothing.
fn parse_duree(text: &str) -> Option<i64> {
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let quantite: i64 = number.parse().ok()?;
    let facteur = match unit {
        "s" => 1_000,
        "m" => 60 * 1_000,
        "h" => 60 * 60 * 1_000,
        "d" => 24 * 60 * 60 * 1_000,
        _ => return None,
    };
    quantite.checked_mul(facteur)
}

fn parse_instant(text: &str) -> Option<i64> {
    // With a zone: the date carries its own offset, nothing to guess.
    if let Ok(ts) = DateTime::parse_from_rfc3339(text) {
        return Some(ts.timestamp_millis());
    }

    // With no zone: the machine's is taken, which is also the logs' in the
    // vast majority of cases.
    const DATES: [&str; 5] = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    ];
    for format in DATES {
        let naive = if format == "%Y-%m-%d" {
            NaiveDate::parse_from_str(text, format)
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        } else {
            NaiveDateTime::parse_from_str(text, format).ok()
        };
        if let Some(naive) = naive {
            return local_ms(naive);
        }
    }

    // An hour on its own means today: "--since 14:30" is what you type on the
    // day itself, in the middle of an incident.
    for format in ["%H:%M:%S", "%H:%M"] {
        if let Ok(heure) = NaiveTime::parse_from_str(text, format) {
            return local_ms(Local::now().date_naive().and_time(heure));
        }
    }
    None
}

/// Reads a zone-less date in the machine's zone.
///
/// `earliest()` settles the two awkward daylight-saving cases: an hour that
/// does not exist in spring, an hour that exists twice in autumn.
fn local_ms(naive: NaiveDateTime) -> Option<i64> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_durations_count_back_from_start_up() {
        assert_eq!(parse_bound("30s"), Ok(Bound::Ago(30_000)));
        assert_eq!(parse_bound("15m"), Ok(Bound::Ago(900_000)));
        assert_eq!(parse_bound("2h"), Ok(Bound::Ago(7_200_000)));
        assert_eq!(parse_bound("3d"), Ok(Bound::Ago(259_200_000)));

        // Starting at noon sharp, "since 15 minutes": 11:45.
        let midi = 1_757_412_000_000;
        assert_eq!(Bound::Ago(900_000).epoch_ms(midi), midi - 900_000);
        assert_eq!(Bound::At(42).epoch_ms(midi), 42);
    }

    #[test]
    fn a_duration_with_no_unit_is_refused() {
        // "--since 15" means nothing: minutes? seconds? We refuse rather than
        // guess.
        assert!(parse_bound("15").is_err());
        assert!(parse_bound("15x").is_err());
        assert!(parse_bound("").is_err());
        assert!(parse_bound("hier matin").is_err());
    }

    #[test]
    fn absolute_dates_are_understood_in_their_usual_forms() {
        let reference = DateTime::parse_from_rfc3339("2026-09-09T14:30:00+02:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            parse_bound("2026-09-09T14:30:00+02:00"),
            Ok(Bound::At(reference))
        );

        // With no zone: the machine's. So we do not compare against a
        // hard-coded value, but against the same date through the same path.
        for text in [
            "2026-09-09T14:30:00",
            "2026-09-09 14:30:00",
            "2026-09-09T14:30",
            "2026-09-09 14:30",
        ] {
            let attendu = local_ms(
                NaiveDate::from_ymd_opt(2026, 9, 9)
                    .unwrap()
                    .and_hms_opt(14, 30, 0)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(parse_bound(text), Ok(Bound::At(attendu)), "{text}");
        }

        // A date on its own starts at midnight.
        let minuit = local_ms(
            NaiveDate::from_ymd_opt(2026, 9, 9)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parse_bound("2026-09-09"), Ok(Bound::At(minuit)));
    }

    #[test]
    fn an_hour_on_its_own_means_today() {
        let attendu = local_ms(Local::now().date_naive().and_hms_opt(14, 30, 0).unwrap()).unwrap();
        assert_eq!(parse_bound("14:30"), Ok(Bound::At(attendu)));
        assert_eq!(parse_bound("14:30:00"), Ok(Bound::At(attendu)));
    }

    #[test]
    fn since_forces_reading_from_the_start_unless_n_caps_it() {
        // Following, with no window: start from the end, like `tail -f`.
        let cli = Cli::parse_from(["refrain", "prod.log"]);
        assert!(!cli.read_from_start());

        // With `--since`, starting from the end would show nothing.
        let cli = Cli::parse_from(["refrain", "--since", "15m", "prod.log"]);
        assert!(cli.read_from_start());

        // Unless `-n` explicitly caps the re-read: that is the cost guard on a
        // forty-gigabyte file.
        let cli = Cli::parse_from(["refrain", "--since", "15m", "-n", "1000", "prod.log"]);
        assert!(!cli.read_from_start());
    }
}
