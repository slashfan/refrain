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
    /// Number of entries analysed.
    Entries,
    /// Duration quantiles, in milliseconds.
    P50,
    P95,
    P99,
    Max,
}

impl Metric {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "error-rate" => Metric::ErrorRate,
            "request-error-rate" => Metric::RequestErrorRate,
            "5xx-rate" => Metric::Rate5xx,
            "errors" => Metric::Errors,
            "entries" => Metric::Entries,
            "p50" => Metric::P50,
            "p95" => Metric::P95,
            "p99" => Metric::P99,
            "max" => Metric::Max,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Metric::ErrorRate => "error-rate",
            Metric::RequestErrorRate => "request-error-rate",
            Metric::Rate5xx => "5xx-rate",
            Metric::Errors => "errors",
            Metric::Entries => "entries",
            Metric::P50 => "p50",
            Metric::P95 => "p95",
            Metric::P99 => "p99",
            Metric::Max => "max",
        }
    }

    /// A duration reads in milliseconds, a rate in percent, a count as an
    /// integer: that is what decides the default unit and the display.
    fn is_duration(self) -> bool {
        matches!(self, Metric::P50 | Metric::P95 | Metric::P99 | Metric::Max)
    }

    /// A share, between 0 and 1: it is written as a percentage and read as one.
    fn is_rate(self) -> bool {
        matches!(
            self,
            Metric::ErrorRate | Metric::RequestErrorRate | Metric::Rate5xx
        )
    }

    /// Can it be restricted to a route? Quantiles, yes, by construction; the
    /// 5xx rate too, since it is counted per endpoint. The global rates, no:
    /// they cover every entry.
    fn allows_endpoint(self) -> bool {
        self.is_duration() || self == Metric::Rate5xx
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
                 (error-rate, request-error-rate, 5xx-rate, errors, entries, \
                  p50, p95, p99, max)"
            )
        })?;
        if endpoint.is_some() && !metric.allows_endpoint() {
            return Err(format!(
                "'{}' covers every entry: it cannot be restricted to one endpoint",
                metric.name()
            ));
        }

        let value = parse_value(right, metric)?;
        Ok(Threshold {
            metric,
            endpoint,
            comparison,
            value,
        })
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
