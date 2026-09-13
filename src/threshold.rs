//! The `--fail-if` thresholds: how they are written, and their verdict.
//!
//! A report from cron or CI is worthless if you have to read it to learn that
//! things are going badly. A crossed threshold must fail the job — plainly,
//! with an exit code distinct from that of an unreadable source.

use crate::stats::{Stats, format_count, format_ms};
use std::fmt;

/// What we measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Share of entries in error, between 0 and 1.
    ErrorRate,
    /// Error lines divided by HTTP requests. Insensitive to logging volume:
    /// this is the threshold you keep in CI when `doctrine.log` is handed over
    /// to be read as well.
    RequestErrorRate,
    /// Share of HTTP responses in 5xx. What the logging level does not say: a
    /// 500 caught and logged at `info` is one, a hundred 404s on
    /// `/favicon.ico` are not.
    Rate5xx,
    /// Number of entries in error.
    Errors,
    /// Number of deprecation lines — occurrences, the way Symfony's own
    /// PHPUnit bridge counts them, not distinct notices. The threshold for a
    /// test suite before an upgrade: `deprecations>0` fails the build on the
    /// first one.
    Deprecations,
    /// Number of entries analysed.
    Entries,
    /// Number of N+1 patterns detected — the rows of the SQL tab, one per
    /// (endpoint, query) pair. This is the threshold for a test suite: run it
    /// with Doctrine logging on, and an N+1 fails the build instead of
    /// waiting for someone to open the profiler.
    Nplus1,
    /// Duration quantiles, in milliseconds.
    P50,
    P95,
    P99,
    Max,
    /// The same quantiles, on the outbound HTTP calls. They apply to the worst
    /// call shape, the way the ones above apply to the worst endpoint: a
    /// provider whose p95 moved is the explanation for a p95 of your own that
    /// moved with it, and the message names the provider.
    HttpP50,
    HttpP95,
    HttpP99,
    HttpMax,
}

impl Metric {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "error-rate" => Metric::ErrorRate,
            "request-error-rate" => Metric::RequestErrorRate,
            "5xx-rate" => Metric::Rate5xx,
            "errors" => Metric::Errors,
            "deprecations" => Metric::Deprecations,
            "entries" => Metric::Entries,
            "nplus1" => Metric::Nplus1,
            "p50" => Metric::P50,
            "p95" => Metric::P95,
            "p99" => Metric::P99,
            "max" => Metric::Max,
            "http-client-p50" => Metric::HttpP50,
            "http-client-p95" => Metric::HttpP95,
            "http-client-p99" => Metric::HttpP99,
            "http-client-max" => Metric::HttpMax,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Metric::ErrorRate => "error-rate",
            Metric::RequestErrorRate => "request-error-rate",
            Metric::Rate5xx => "5xx-rate",
            Metric::Errors => "errors",
            Metric::Deprecations => "deprecations",
            Metric::Entries => "entries",
            Metric::Nplus1 => "nplus1",
            Metric::P50 => "p50",
            Metric::P95 => "p95",
            Metric::P99 => "p99",
            Metric::Max => "max",
            Metric::HttpP50 => "http-client-p50",
            Metric::HttpP95 => "http-client-p95",
            Metric::HttpP99 => "http-client-p99",
            Metric::HttpMax => "http-client-max",
        }
    }

    /// Does it measure an outbound call rather than a request of our own?
    fn is_http_client(self) -> bool {
        matches!(
            self,
            Metric::HttpP50 | Metric::HttpP95 | Metric::HttpP99 | Metric::HttpMax
        )
    }

    /// A duration reads in milliseconds, a rate in percent, a count as an
    /// integer: that is what decides the default unit and the display.
    fn is_duration(self) -> bool {
        matches!(self, Metric::P50 | Metric::P95 | Metric::P99 | Metric::Max)
            || self.is_http_client()
    }

    /// A share, between 0 and 1: it is written as a percentage and read as one.
    fn is_rate(self) -> bool {
        matches!(
            self,
            Metric::ErrorRate | Metric::RequestErrorRate | Metric::Rate5xx
        )
    }

    /// Can it be restricted to a route? Quantiles, yes, by construction; the
    /// 5xx rate and the N+1 patterns too, since they are counted per
    /// endpoint. The global rates, no: they cover every entry. Nor the
    /// outbound calls: they are grouped by provider, not by route, and their
    /// shape already carries a `:` when the host names a port.
    fn allows_endpoint(self) -> bool {
        !self.is_http_client()
            && (self.is_duration() || matches!(self, Metric::Rate5xx | Metric::Nplus1))
    }

    fn format(self, value: f64) -> String {
        if self.is_duration() {
            format_ms(value as f32)
        } else if self.is_rate() {
            format!("{:.2} %", value * 100.0)
        } else {
            format_count(value as u64)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Comparison {
    fn holds(self, measured: f64, threshold: f64) -> bool {
        match self {
            Comparison::Gt => measured > threshold,
            Comparison::Ge => measured >= threshold,
            Comparison::Lt => measured < threshold,
            Comparison::Le => measured <= threshold,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Comparison::Gt => ">",
            Comparison::Ge => ">=",
            Comparison::Lt => "<",
            Comparison::Le => "<=",
        }
    }
}

/// A threshold: `p95:api_orders_list>1s`.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    metric: Metric,
    /// The endpoint aimed at. Absent, a quantile covers the **worst** endpoint:
    /// "no route may go over one second at p95" is what you mean in CI, and the
    /// message will name the culprit.
    endpoint: Option<String>,
    comparison: Comparison,
    value: f64,
}

impl fmt::Display for Threshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.metric.name())?;
        if let Some(endpoint) = &self.endpoint {
            write!(f, ":{endpoint}")?;
        }
        write!(
            f,
            "{}{}",
            self.comparison.symbol(),
            self.metric.format(self.value)
        )
    }
}

/// What gets written when a threshold is crossed.
pub struct Breach {
    pub threshold: Threshold,
    pub measured: f64,
    /// The endpoint actually at fault, when the threshold named none.
    pub culprit: Option<String>,
}

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metric = self.threshold.metric;
        write!(f, "{}", metric.name())?;
        if let Some(endpoint) = self.threshold.endpoint.as_ref().or(self.culprit.as_ref()) {
            write!(f, " ({endpoint})")?;
        }
        write!(
            f,
            " = {} {} {}",
            metric.format(self.measured),
            self.threshold.comparison.symbol(),
            metric.format(self.threshold.value)
        )
    }
}

impl Threshold {
    /// `error-rate>2%`, `p95:api_orders_list>1s`, `entries<100`.
    ///
    /// Parsed when the program opens and not at the end: a malformed threshold
    /// must fail straight away, not after reading forty gigabytes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        // The two-character ones first: otherwise ">=" would be cut on its ">".
        let (index, comparison) = [
            (">=", Comparison::Ge),
            ("<=", Comparison::Le),
            (">", Comparison::Gt),
            ("<", Comparison::Lt),
        ]
        .iter()
        .find_map(|(motif, comparison)| text.find(motif).map(|i| (i, (*comparison, motif.len()))))
        .ok_or_else(|| format!("'{text}' has no comparator (>, >=, <, <=)"))?;
        let (comparison, width) = comparison;

        let left = &text[..index];
        let right = text[index + width..].trim();

        let (name, endpoint) = match left.trim().split_once(':') {
            Some((name, endpoint)) => (name.trim(), Some(endpoint.trim().to_string())),
            None => (left.trim(), None),
        };
        let metric = Metric::parse(name).ok_or_else(|| {
            format!(
                "'{name}' is not a known metric \
                 (error-rate, request-error-rate, 5xx-rate, errors, deprecations, \
                  entries, nplus1, p50, p95, p99, max, \
                  http-client-p50, http-client-p95, http-client-p99, http-client-max)"
            )
        })?;
        if endpoint.is_some() && !metric.allows_endpoint() {
            return Err(match metric.is_http_client() {
                true => format!(
                    "'{}' is grouped by provider, not by endpoint: it takes none",
                    metric.name()
                ),
                false => format!(
                    "'{}' covers every entry: it cannot be restricted to one endpoint",
                    metric.name()
                ),
            });
        }

        let value = parse_value(right, metric)?;
        Ok(Threshold {
            metric,
            endpoint,
            comparison,
            value,
        })
    }

    /// Does the threshold rely on N+1 detection? `--nplus1 0` switches that
    /// detection off, and a threshold on it would then look respected for
    /// ever: the command line contradicts itself, and is refused at start-up.
    pub fn counts_nplus1(&self) -> bool {
        self.metric == Metric::Nplus1
    }

    /// Is the threshold crossed? Returns what to write, or `None`.
    pub fn check(&self, stats: &Stats) -> Option<Breach> {
        let (measured, culprit) = self.measure(stats)?;
        self.comparison.holds(measured, self.value).then(|| Breach {
            threshold: self.clone(),
            measured,
            culprit,
        })
    }

    fn measure(&self, stats: &Stats) -> Option<(f64, Option<String>)> {
        let simple = match self.metric {
            // Nothing analysed: zero lines is not zero errors out of
            // something. The one rate that still answered here.
            Metric::ErrorRate => Some(match stats.total {
                0 => return None,
                total => stats.errors_total() as f64 / total as f64,
            }),
            // `None`: no request seen, nothing to say — and above all not a
            // zero that would make the threshold look respected.
            Metric::RequestErrorRate => Some(stats.request_error_rate()?),
            Metric::Errors => Some(stats.errors_total() as f64),
            // Lines, like `errors`: a ceiling on the detailed table never
            // stops this count.
            Metric::Deprecations => Some(stats.deprecations_total as f64),
            Metric::Entries => Some(stats.total as f64),
            _ => None,
        };
        if let Some(value) = simple {
            return Some((value, None));
        }

        // The only rate that is also counted per endpoint. With no status read
        // — neither for the named route nor anywhere — we do not pronounce.
        if self.metric == Metric::Rate5xx {
            return match &self.endpoint {
                Some(name) => Some((stats.routes.get(name)?.rate_5xx()?, None)),
                None => Some((stats.rate_5xx()?, None)),
            };
        }

        // N+1 patterns are only found in SQL queries: with none read at all —
        // `doctrine.log` not handed over, Doctrine not logging — zero patterns
        // is not a clean bill of health, it is an absence of information, and
        // the threshold does not pronounce. Same silence for an endpoint never
        // seen. A route that was seen and has no pattern, itself, does answer
        // zero: that is the point of the threshold.
        if self.metric == Metric::Nplus1 {
            if stats.sql_shapes() == 0 {
                return None;
            }
            return match &self.endpoint {
                Some(name) => {
                    stats.routes.get(name)?;
                    let count = stats
                        .nplus1
                        .keys()
                        .filter(|(endpoint, _)| endpoint == name)
                        .count();
                    Some((count as f64, None))
                }
                None => Some((stats.nplus1.len() as f64, None)),
            };
        }

        // The outbound calls: the worst shape, the way a quantile with no
        // endpoint is the worst endpoint. With none timed — the application
        // logs no `total_time`, or makes no outbound call at all — nothing is
        // said rather than a reassuring zero, the same rule as `5xx-rate`
        // with no status read.
        if self.metric.is_http_client() {
            return stats
                .http
                .values()
                .filter(|shape| shape.timed > 0)
                .map(|shape| {
                    let value = match self.metric {
                        Metric::HttpMax => f64::from(shape.max_ms),
                        Metric::HttpP50 => f64::from(shape.quantiles().p50),
                        Metric::HttpP95 => f64::from(shape.quantiles().p95),
                        _ => f64::from(shape.quantiles().p99),
                    };
                    (value, Some(shape.shape.clone()))
                })
                .max_by(|a, b| a.0.total_cmp(&b.0));
        }

        let quantile = |route: &crate::stats::RouteStat| -> f64 {
            match self.metric {
                Metric::Max => route.max_ms as f64,
                Metric::P50 => route.quantiles().p50 as f64,
                Metric::P95 => route.quantiles().p95 as f64,
                Metric::P99 => route.quantiles().p99 as f64,
                _ => unreachable!("the simple metrics are handled above"),
            }
        };

        if let Some(name) = &self.endpoint {
            // An endpoint named but absent from the logs: nothing can be said,
            // and inventing a zero would make the threshold look respected.
            let route = stats.routes.get(name)?;
            return Some((quantile(route), None));
        }

        // With no endpoint: the worst of them all. Only the routes with at
        // least one measured duration are kept, otherwise their zero would drag
        // the maximum down and hide the one slow route.
        stats
            .routes
            .iter()
            .filter(|(_, route)| route.timed > 0)
            .map(|(name, route)| (quantile(route), Some(name.clone())))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// `2%` → 0.02, `1s` → 1000 ms, `500ms` → 500, `100` → 100.
fn parse_value(text: &str, metric: Metric) -> Result<f64, String> {
    let invalid = || format!("'{text}' is not a valid value");

    if let Some(number) = text.strip_suffix('%') {
        let value: f64 = number.trim().parse().map_err(|_| invalid())?;
        if !metric.is_rate() {
            return Err(format!(
                "a percentage makes no sense for '{}'",
                metric.name()
            ));
        }
        return Ok(value / 100.0);
    }

    // Order matters: "ms" before "s", otherwise "500ms" would read as "500m".
    for (suffix, factor) in [("ms", 1.0), ("s", 1000.0)] {
        if let Some(number) = text.strip_suffix(suffix) {
            if !metric.is_duration() {
                return Err(format!("a duration makes no sense for '{}'", metric.name()));
            }
            let value: f64 = number.trim().parse().map_err(|_| invalid())?;
            return Ok(value * factor);
        }
    }

    // With no unit: milliseconds for a duration, the raw value otherwise. A
    // rate is then written as a fraction — "error-rate>0.02" means ">2%".
    text.parse().map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::parser::parse_line;
    use clap::Parser;

    fn parsed(text: &str) -> Threshold {
        Threshold::parse(text).unwrap_or_else(|e| panic!("« {text} » : {e}"))
    }

    /// Two endpoints, one slow and faulty, the other fast and healthy.
    fn test_stats() -> Stats {
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        let lignes = [
            (r#"{"route":"slow","duration_ms":2000.0}"#, "INFO"),
            (r#"{"route":"slow","duration_ms":3000.0}"#, "INFO"),
            (r#"{"route":"fast","duration_ms":10.0}"#, "INFO"),
            (r#"{"route":"fast","duration_ms":20.0}"#, "INFO"),
            (r#"{"route":"slow"}"#, "CRITICAL"),
        ];
        for (context, level) in lignes {
            let line =
                format!(r#"[2026-09-09T10:00:00.000000+02:00] request.{level}: Fini {context} []"#);
            stats.ingest(0, parse_line(&line).expect("line valide"));
        }
        stats.finalize();
        stats
    }

    #[test]
    fn the_grammar_accepts_what_it_advertises() {
        // The units all reduce to the same internal scale.
        assert_eq!(parsed("error-rate>2%"), parsed("error-rate>0.02"));
        assert_eq!(parsed("p95>1s"), parsed("p95>1000"));
        assert_eq!(parsed("p95>1s"), parsed("p95>1000ms"));

        // Two-character comparators must not be cut on their first: ">=" is
        // not ">" followed by "=2%".
        assert_eq!(parsed("errors>=10").comparison, Comparison::Ge);
        assert_eq!(parsed("entries<=10").comparison, Comparison::Le);
        assert_eq!(parsed("entries<100").comparison, Comparison::Lt);

        // Surrounding spaces do not get in the way: the value often comes from
        // a configuration file or an environment variable.
        assert_eq!(
            parsed(" p95 : app_home > 800 ms "),
            parsed("p95:app_home>800ms")
        );

        // And the threshold is written back the way it was understood.
        assert_eq!(
            parsed("p95:app_home>800ms").to_string(),
            "p95:app_home>800 ms"
        );
    }

    #[test]
    fn the_grammar_refuses_what_makes_no_sense() {
        assert!(
            Threshold::parse("p95 is too large").is_err(),
            "no comparator"
        );
        assert!(
            Threshold::parse("tps_reponse>1s").is_err(),
            "unknown metric"
        );
        assert!(Threshold::parse("p95>vite").is_err(), "non-numeric value");
        // A percentage of milliseconds, a duration of entries: no.
        assert!(Threshold::parse("p95>2%").is_err());
        assert!(Threshold::parse("entries>2s").is_err());
        // A global rate is not restricted to an endpoint.
        assert!(Threshold::parse("error-rate:app_home>2%").is_err());
        assert!(Threshold::parse("request-error-rate:app_home>2%").is_err());
        // The 5xx one, however, can be: it is counted per endpoint.
        assert!(Threshold::parse("5xx-rate:app_home>1%").is_ok());

        // The outbound quantiles are grouped by provider, not by endpoint.
        assert!(Threshold::parse("http-client-p95>1s").is_ok());
        assert!(Threshold::parse("http-client-max>2500ms").is_ok());
        let refused = Threshold::parse("http-client-p95:app_home>1s").expect_err("no endpoint");
        assert!(refused.contains("by provider"), "{refused}");
    }

    #[test]
    fn the_outbound_quantile_names_the_slowest_provider() {
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        // Two providers, one of them slow; and an endpoint of our own that is
        // fast, so the threshold cannot be reading the wrong figure.
        let calls = [
            ("https://api.slow.test/v1/charge", 1.8),
            ("https://api.slow.test/v1/charge", 2.2),
            ("https://api.quick.test/v1/ping", 0.01),
        ];
        for (url, seconds) in calls {
            let line = format!(
                r#"[2026-09-09T10:00:00.000000+02:00] http_client.INFO: Response: "200 {url}" {seconds:.6} seconds {{"http_method":"POST","http_code":200,"total_time":{seconds:.6}}} []"#
            );
            stats.ingest(0, parse_line(&line).expect("valid line"));
        }
        stats.finalize();

        // With no provider named, the worst of them — and the message says
        // which, the way a quantile with no endpoint names the culprit route.
        let breach = parsed("http-client-p95>1s")
            .check(&stats)
            .expect("one provider goes over a second");
        assert!(
            breach
                .to_string()
                .starts_with("http-client-p95 (POST api.slow.test/v1/charge) ="),
            "{breach}"
        );
        assert!(parsed("http-client-p95>3s").check(&stats).is_none());
        assert!(parsed("http-client-max>2s").check(&stats).is_some());
    }

    #[test]
    fn an_outbound_threshold_says_nothing_when_nothing_was_measured() {
        // No outbound call read at all — the channel never reaches a file —
        // or calls read with no `total_time` on them: zero is not a clean bill
        // of health, it is an absence of information. Same rule as `5xx-rate`
        // with no status read.
        let empty = test_stats();
        assert!(parsed("http-client-p95>0").check(&empty).is_none());

        let mut untimed = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        let line = r#"[2026-09-09T10:00:00.000000+02:00] http_client.INFO: Response: "200 https://api.test/v1/ping" [] []"#;
        untimed.ingest(0, parse_line(line).expect("valid line"));
        untimed.finalize();
        assert_eq!(untimed.http_calls, 1, "the call was read");
        assert!(
            parsed("http-client-p95>0").check(&untimed).is_none(),
            "but nothing was timed"
        );
    }

    #[test]
    fn global_thresholds_are_measured_over_everything() {
        let stats = test_stats();

        // Five entries, one of them in error: 20 %.
        assert!(parsed("error-rate>10%").check(&stats).is_some());
        assert!(parsed("error-rate>50%").check(&stats).is_none());
        assert!(parsed("errors>=1").check(&stats).is_some());
        assert!(parsed("entries<3").check(&stats).is_none());

        let breach = parsed("error-rate>10%").check(&stats).unwrap();
        assert_eq!(breach.to_string(), "error-rate = 20.00 % > 10.00 %");
    }

    #[test]
    fn the_two_error_rates_do_not_measure_the_same_thing() {
        let stats = test_stats();

        // Five lines, four of them requests, and one error: 20 % of the lines,
        // but 25 % of the requests. The second denominator is the only one that
        // does not move when `doctrine.log` is added to the reading.
        assert!(
            parsed("error-rate>22%").check(&stats).is_none(),
            "20 % of the lines are in error"
        );
        let breach = parsed("request-error-rate>22%")
            .check(&stats)
            .expect("25 % of the requests are in error");
        assert_eq!(breach.to_string(), "request-error-rate = 25.00 % > 22.00 %");
    }

    #[test]
    fn the_5xx_rate_counts_responses_and_not_levels() {
        // Four responses, all logged at `info`: no error level at all, and yet
        // one outage in four.
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        for (route, status) in [("slow", 500), ("slow", 200), ("fast", 200), ("fast", 404)] {
            let line = format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Request finished {{"route":"{route}","status":{status},"duration_ms":10}} []"#
            );
            stats.ingest(0, parse_line(&line).expect("line valide"));
        }
        stats.finalize();

        assert!(
            parsed("errors>=1").check(&stats).is_none(),
            "no error level at all"
        );
        assert!(
            parsed("5xx-rate>20%").check(&stats).is_some(),
            "one in four"
        );
        assert!(parsed("5xx-rate>30%").check(&stats).is_none());

        // And it is "slow" that carries it: one of its two responses.
        let breach = parsed("5xx-rate:slow>40%").check(&stats).expect("crossed");
        assert_eq!(breach.to_string(), "5xx-rate (slow) = 50.00 % > 40.00 %");
        assert!(parsed("5xx-rate:fast>1%").check(&stats).is_none());
        // The 404 on "fast" is not an outage.
        assert!(parsed("5xx-rate:unknown_route>0%").check(&stats).is_none());

        // With no status logged at all, the threshold does not pronounce.
        let without_status = test_stats();
        assert!(parsed("5xx-rate>0%").check(&without_status).is_none());
    }

    #[test]
    fn with_no_request_the_per_request_rate_declares_nothing() {
        // A log where nothing marks an HTTP request: no "Matched route", no
        // duration. Returning 0 % would make the threshold look respected when
        // we have nothing to say about it — the rule already followed for a
        // missing endpoint.
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        let line = r#"[2026-09-09T10:00:00.000000+02:00] app.CRITICAL: Boum {} []"#;
        stats.ingest(0, parse_line(line).expect("line valide"));
        stats.finalize();

        assert!(parsed("request-error-rate>0%").check(&stats).is_none());
        // The global rate, itself, does pronounce: that line is an error.
        assert!(parsed("error-rate>99%").check(&stats).is_some());
    }

    #[test]
    fn with_no_line_the_error_rate_declares_nothing() {
        // A wrong path that exists, a log rotated a second ago, a window with
        // nothing in it: `error-rate` used to answer 0 %, and "error-rate<1%"
        // was crossed on an empty file while "error-rate>2%" looked respected.
        // Neither has anything to say — the rule the other two rates already
        // followed.
        let empty = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        assert!(parsed("error-rate<1%").check(&empty).is_none());
        assert!(parsed("error-rate>=0%").check(&empty).is_none());
        // A single line, and it answers again.
        let stats = test_stats();
        assert!(parsed("error-rate>=0%").check(&stats).is_some());
    }

    /// One HTTP request per token, each running the same prepared statement
    /// `repeats` times. `app_orders` loops, `app_home` does not.
    fn stats_with_sql(repeats: &[(&str, &str, usize)]) -> Stats {
        let mut stats = Stats::new(&Cli::parse_from(["refrain", "prod.log"]));
        for (token, route, n) in repeats {
            let mut lines = vec![format!(
                r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "{route}". {{"route":"{route}"}} {{"token":"{token}"}}"#
            )];
            for _ in 0..*n {
                lines.push(format!(
                    r#"[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT t0.id FROM customer t0 WHERE t0.id = ?","params":{{"1":1}}}} {{"token":"{token}"}}"#
                ));
            }
            lines.push(format!(
                r#"[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished {{"route":"{route}","status":200,"duration_ms":120.0}} {{"token":"{token}"}}"#
            ));
            for line in lines {
                stats.ingest(0, parse_line(&line).expect("valid line"));
            }
        }
        stats.finalize();
        stats
    }

    #[test]
    fn an_nplus1_fails_the_build() {
        // Two routes, one of them looping twelve times over the same query:
        // one pattern, and it belongs to app_orders.
        let stats = stats_with_sql(&[("a", "app_orders", 12), ("b", "app_home", 1)]);
        assert_eq!(stats.nplus1.len(), 1, "one pattern detected");

        let breach = parsed("nplus1>0").check(&stats).expect("crossed");
        assert_eq!(breach.to_string(), "nplus1 = 1 > 0");
        assert!(parsed("nplus1>1").check(&stats).is_none());

        let breach = parsed("nplus1:app_orders>0")
            .check(&stats)
            .expect("crossed");
        assert_eq!(breach.to_string(), "nplus1 (app_orders) = 1 > 0");
        // A route seen, with no pattern: zero is a genuine answer here, so
        // "no more than zero" holds and "at least one" is not crossed.
        assert!(parsed("nplus1:app_home>0").check(&stats).is_none());
        assert!(parsed("nplus1:app_home<1").check(&stats).is_some());
    }

    #[test]
    fn with_no_sql_read_the_nplus1_count_declares_nothing() {
        // Requests, but not one SQL line: `doctrine.log` was not handed over,
        // or Doctrine is not logging. Zero patterns would pass the build for
        // the wrong reason.
        let stats = test_stats();
        assert!(parsed("nplus1>0").check(&stats).is_none());
        assert!(parsed("nplus1<1").check(&stats).is_none());
        assert!(parsed("nplus1:slow<1").check(&stats).is_none());

        // And an endpoint never seen declares nothing either, even with SQL
        // read elsewhere — the rule the quantiles already follow.
        let stats = stats_with_sql(&[("a", "app_orders", 12)]);
        assert!(parsed("nplus1:never_seen>0").check(&stats).is_none());
        assert!(parsed("nplus1:never_seen<1").check(&stats).is_none());
    }

    #[test]
    fn a_deprecation_fails_the_build() {
        let mut stats = test_stats();
        // The same deprecation three times, once per request: three lines,
        // one distinct — and it is the lines that are counted, the way the
        // PHPUnit bridge's `max[total]` does.
        for _ in 0..3 {
            let line = r#"[2026-09-09T10:00:00.000000+02:00] php.INFO: User Deprecated: Since app 2.0: The "Legacy" class is deprecated. {"exception":"[object] (ErrorException(code: 0): User Deprecated: Since app 2.0: The \"Legacy\" class is deprecated. at /var/www/src/Legacy.php:12)"} []"#;
            stats.ingest(0, parse_line(line).expect("line valide"));
        }
        assert_eq!(stats.deprecations.len(), 1);

        let breach = parsed("deprecations>0").check(&stats).expect("crossed");
        assert_eq!(breach.to_string(), "deprecations = 3 > 0");
        assert!(parsed("deprecations>=3").check(&stats).is_some());
        assert!(parsed("deprecations>3").check(&stats).is_none());

        // None read: a plain zero, like `errors` — a log with no deprecation
        // in it is the very thing the threshold is there to certify.
        assert!(parsed("deprecations>0").check(&test_stats()).is_none());
        assert!(parsed("deprecations<1").check(&test_stats()).is_some());

        // Counted over every entry, like `errors`: no endpoint, and neither a
        // rate nor a duration.
        assert!(Threshold::parse("deprecations:app_home>0").is_err());
        assert!(Threshold::parse("deprecations>2%").is_err());
        assert!(Threshold::parse("deprecations>2s").is_err());
    }

    #[test]
    fn an_nplus1_count_is_an_integer() {
        assert!(Threshold::parse("nplus1>0").is_ok());
        assert!(Threshold::parse("nplus1:app_orders>=2").is_ok());
        assert!(parsed("nplus1>0").counts_nplus1());
        assert!(!parsed("p95>1s").counts_nplus1());
        // Neither a rate nor a duration.
        assert!(Threshold::parse("nplus1>2%").is_err());
        assert!(Threshold::parse("nplus1>2s").is_err());
    }

    #[test]
    fn with_no_endpoint_a_quantile_aims_at_the_worst() {
        let stats = test_stats();

        // "slow" tops out at 3 s, "fast" at 20 ms: the worst decides, and the
        // message must name it.
        let breach = parsed("max>1s").check(&stats).expect("crossed");
        assert_eq!(breach.culprit.as_deref(), Some("slow"));
        assert!(breach.to_string().contains("max (slow)"), "{breach}");

        assert!(parsed("max>10s").check(&stats).is_none());

        // Naming the healthy endpoint makes the threshold respected, where the
        // worst one crossed it: the route asked for is indeed the one measured.
        assert!(parsed("max:fast>1s").check(&stats).is_none());
        assert!(parsed("max:slow>1s").check(&stats).is_some());
    }

    #[test]
    fn a_missing_endpoint_declares_nothing() {
        let stats = test_stats();
        // Inventing a zero would make the threshold look respected, which is a
        // lie: we do not pronounce.
        assert!(parsed("p95:jamais_vu>1ms").check(&stats).is_none());
        assert!(parsed("p95:jamais_vu<1ms").check(&stats).is_none());
    }
}
