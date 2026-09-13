//! Exporting the selected item: the text of a report, writing it to a file,
//! copying it to the clipboard.
//!
//! When you finally hold the error, you want to paste it into a ticket. Going
//! back to hunt for it by hand in forty gigabytes of log would undo the benefit
//! of having found it here.

use crate::app::{App, Tab};
use crate::stats::{format_count, format_ms, format_time, render_summary};
use anyhow::{Context, Result};
use chrono::Local;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// A report ready to be written or pasted.
pub struct Report {
    pub text: String,
    /// Readable file name, without extension:
    /// `refrain-error-ProductNotFound-20260909-231205`.
    pub slug: String,
}

/// The report of the item selected in the current tab.
///
/// The tabs with no selection — the overview, the stream — return the full
/// summary: `w` therefore always does something useful, rather than nothing.
pub fn report(app: &App) -> Report {
    match app.tab {
        Tab::Errors => error_report(app),
        Tab::Endpoints => endpoint_report(app),
        Tab::Sql => nplus1_report(app),
        Tab::Outbound => outbound_report(app),
        Tab::Deprecations => deprecation_report(app),
        Tab::Overview | Tab::Stream => Report {
            text: with_header(app, "summary", render_summary(&app.stats)),
            slug: slug("summary", None),
        },
    }
}

/// Writes the report into `dir` and returns its path.
pub fn write_to(app: &App, dir: &Path) -> Result<PathBuf> {
    let report = report(app);
    let path = dir.join(format!("{}.txt", report.slug));
    std::fs::write(&path, report.text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The OSC 52 escape sequence, which asks the **terminal** to put this text in
/// the clipboard.
///
/// It is the only way through an `ssh`: the clipboard aimed at is the one on
/// the machine you are looking at, not the one on the server refrain runs on.
/// No dependency either, where a clipboard library would drag in X11 or Wayland
/// on Linux.
///
/// Not every terminal honours it — Terminal.app ignores it, tmux wants
/// `set -g set-clipboard on` — and some cap the size of what they accept.
/// Hence `w`, which depends on nobody.
pub fn clipboard_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

// ---------------------------------------------------------------------------
// The reports
// ---------------------------------------------------------------------------

fn error_report(app: &App) -> Report {
    let Some(row) = app.error_rows.get(app.error_sel) else {
        return empty("error");
    };
    let Some(stat) = app.stats.errors.get(&row.signature) else {
        return empty("error");
    };

    let mut out = String::new();
    let _ = writeln!(out, "signature : {}", row.signature);
    let _ = writeln!(
        out,
        "seen      : {} times, from {} to {}",
        format_count(stat.count),
        format_time(stat.first_seen),
        format_time(stat.last_seen)
    );
    let _ = writeln!(out, "level     : {}", stat.level.as_str());
    let _ = writeln!(out, "channel   : {}", stat.channel);
    if let Some(exception) = &stat.exception {
        let _ = writeln!(out, "exception : {exception}");
    }
    if let Some(endpoint) = &stat.endpoint {
        let _ = writeln!(out, "endpoint  : {endpoint}");
    }

    // The message carries the stack trace: the continuation lines were
    // attached to it at parse time. That is the whole point of exporting —
    // the screen, itself, shows only its first three lines.
    let _ = writeln!(out, "\nLatest occurrence\n{}", stat.message);
    if let Some(context) = &stat.context {
        let _ = writeln!(out, "\nContext\n{context}");
    }

    let name = stat
        .exception
        .as_deref()
        .map(crate::parser::short_class)
        .unwrap_or(&stat.channel);
    Report {
        text: with_header(app, "error", out),
        slug: slug("error", Some(name)),
    }
}

fn endpoint_report(app: &App) -> Report {
    let Some(row) = app.route_rows.get(app.route_sel) else {
        return empty("endpoint");
    };

    let mut out = String::new();
    let _ = writeln!(out, "endpoint : {}", row.name);
    let _ = writeln!(
        out,
        "requests : {} ({} failed, {:.1} %)",
        format_count(row.requests),
        format_count(row.errors),
        row.error_rate * 100.0
    );
    if row.timed > 0 {
        let _ = writeln!(
            out,
            "durations: p50 {} · p95 {} · max {} (over {} timed requests)",
            format_ms(row.p50),
            format_ms(row.p95),
            format_ms(row.max),
            format_count(row.timed)
        );
    } else {
        let _ = writeln!(out, "durations: none measured");
    }
    if row.avg_queries > 0.0 {
        let _ = writeln!(out, "SQL/req  : {:.1} on average", row.avg_queries);
    }
    if row.avg_calls > 0.0 {
        let _ = writeln!(
            out,
            "HTTP/req : {:.1} outbound calls on average",
            row.avg_calls
        );
    }
    let _ = writeln!(out, "measured : {}", app.stats.duration.label());

    // The N+1 patterns of this endpoint: almost always the explanation of a
    // p95 going wrong, so it may as well be in the same clipboard.
    let mut patterns: Vec<_> = app
        .stats
        .nplus1
        .values()
        .filter(|pattern| pattern.endpoint == row.name)
        .collect();
    patterns.sort_unstable_by_key(|pattern| std::cmp::Reverse(pattern.max_count));
    if !patterns.is_empty() {
        let _ = writeln!(out, "\nN+1 patterns for this endpoint");
        for pattern in patterns.iter().take(10) {
            let _ = writeln!(
                out,
                "  {} × at worst, {:.1} on average over {} requests\n    {}",
                pattern.max_count,
                pattern.avg_count(),
                format_count(pattern.requests),
                pattern.sql
            );
        }
    }

    Report {
        text: with_header(app, "endpoint", out),
        slug: slug("endpoint", Some(&row.name)),
    }
}

fn nplus1_report(app: &App) -> Report {
    let Some(row) = app.nplus1_rows.get(app.nplus1_sel) else {
        return empty("n+1");
    };
    let Some(pattern) = app.stats.nplus1.get(&row.key) else {
        return empty("n+1");
    };

    let mut out = String::new();
    let _ = writeln!(out, "endpoint : {}", pattern.endpoint);
    let _ = writeln!(
        out,
        "worst    : {} executions within a single HTTP request",
        pattern.max_count
    );
    let _ = writeln!(
        out,
        "average  : {:.1} over {} HTTP requests affected",
        pattern.avg_count(),
        format_count(pattern.requests)
    );
    let _ = writeln!(out, "last seen: {}", format_time(pattern.last_seen));
    let _ = writeln!(out, "threshold: {} ×", app.cli.nplus1);
    let _ = writeln!(out, "\nRepeated query\n{}", pattern.sql);

    Report {
        text: with_header(app, "N+1 pattern", out),
        slug: slug("nplus1", Some(&pattern.endpoint)),
    }
}

fn outbound_report(app: &App) -> Report {
    let Some(row) = app.outbound_rows.get(app.outbound_sel) else {
        return empty("outbound call");
    };
    let Some(shape) = app.stats.http.get(&row.key) else {
        return empty("outbound call");
    };
    let quantiles = shape.quantiles();

    let mut out = String::new();
    // The shape and nothing else: the query string was dropped when the line
    // was read, and this is one of the places it must not reappear.
    let _ = writeln!(out, "call     : {}", shape.shape);
    let _ = writeln!(out, "calls    : {}", format_count(shape.calls));
    if shape.timed > 0 {
        let _ = writeln!(
            out,
            "durations: p50 {} · p95 {} · p99 {} · max {} (over {} timed calls)",
            format_ms(quantiles.p50),
            format_ms(quantiles.p95),
            format_ms(quantiles.p99),
            format_ms(shape.max_ms),
            format_count(shape.timed)
        );
    } else {
        let _ = writeln!(out, "durations: none measured (no total_time on the lines)");
    }
    if shape.responses > 0 {
        let _ = writeln!(
            out,
            "answers  : {} with a status · {} × 4xx · {} × 5xx",
            format_count(shape.responses),
            format_count(shape.status_4xx),
            format_count(shape.status_5xx)
        );
    }
    if shape.requests > 0 {
        let _ = writeln!(
            out,
            "per req. : {} × at worst, {:.1} on average over {} requests",
            shape.max_per_request,
            shape.avg_per_request(),
            format_count(shape.requests)
        );
    }
    if let Some(endpoint) = &shape.worst_endpoint {
        let _ = writeln!(out, "worst from: {endpoint}");
    }
    let _ = writeln!(out, "last seen: {}", format_time(shape.last_seen));

    Report {
        text: with_header(app, "outbound call", out),
        slug: slug("outbound", Some(&shape.shape)),
    }
}

fn deprecation_report(app: &App) -> Report {
    let Some(row) = app.deprecation_rows.get(app.deprecation_sel) else {
        return empty("deprecation");
    };
    let Some(stat) = app.stats.deprecations.get(&row.key) else {
        return empty("deprecation");
    };

    let mut out = String::new();
    let _ = writeln!(out, "signature : {}", row.key.0);
    let _ = writeln!(
        out,
        "seen      : {} times, from {} to {}",
        format_count(stat.count),
        format_time(stat.first_seen),
        format_time(stat.last_seen)
    );
    let _ = writeln!(out, "channel   : {}", stat.channel);
    if let Some(origin) = &stat.origin {
        let _ = writeln!(out, "origin    : {origin}");
    }
    if let Some(endpoint) = &stat.endpoint {
        let _ = writeln!(out, "last from : {endpoint}");
    }
    let _ = writeln!(out, "\nLatest occurrence\n{}", stat.message);

    // The file is named after the deprecated code, `Request.php`: that is
    // what one searches for next.
    let name = stat
        .origin
        .as_deref()
        .and_then(|origin| origin.rsplit('/').next())
        .map(|file| file.split(':').next().unwrap_or(file))
        .unwrap_or(&stat.channel);
    Report {
        text: with_header(app, "deprecation", out),
        slug: slug("deprecation", Some(name)),
    }
}

fn empty(what: &str) -> Report {
    Report {
        text: format!("Nothing to export: no {what} selected.\n"),
        slug: slug(what, None),
    }
}

/// The common header. A report pasted into a ticket must stand on its own:
/// what you were looking at, when, and where it came from.
fn with_header(app: &App, what: &str, body: String) -> String {
    let sources: Vec<String> = app
        .cli
        .files
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    format!(
        "── refrain ─ {} ────────────────────────────\n\
         exported {}\n\
         sources: {}\n\n{}",
        what,
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        sources.join(", "),
        body
    )
}

// ---------------------------------------------------------------------------
// File name
// ---------------------------------------------------------------------------

fn slug(what: &str, name: Option<&str>) -> String {
    let mut out = format!("refrain-{what}");
    if let Some(name) = name {
        let name = sanitize(name);
        if !name.is_empty() {
            out.push('-');
            out.push_str(&name);
        }
    }
    let _ = write!(out, "-{}", Local::now().format("%Y%m%d-%H%M%S"));
    out
}

/// Reduces a name to what goes through anywhere in a file name. An endpoint is
/// often a URI (`/api/orders/42`), an exception a fully qualified name: both
/// carry characters we do not want here.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(40));
    let mut previous_dash = false;
    for c in name.chars().take(60) {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
            previous_dash = false;
        } else if !previous_dash && !out.is_empty() {
            out.push('-');
            previous_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(40);
    out
}

// ---------------------------------------------------------------------------
// base64, for OSC 52
// ---------------------------------------------------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encoding, with padding. Fifteen-odd lines against one more
/// dependency: the computation is trivial and will never change.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = chunk.get(1).copied().map_or(0, u32::from);
        let b3 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b1 << 16) | (b2 << 8) | b3;

        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::event::Event;
    use crate::parser::parse_line;
    use clap::Parser;

    fn app_with_one_error() -> App {
        let mut app = App::new(Cli::parse_from(["refrain", "var/log/prod.log"]), 1);
        let lignes = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_product_show". {"route":"app_product_show"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.100000+02:00] request.INFO: Request finished {"route":"app_product_show","duration_ms":120.0} {"token":"aaa"}"#,
        ];
        for line in lignes {
            app.stats.ingest(0, parse_line(line).expect("line valide"));
        }

        // An exception with its stack trace, as `tail.rs` sends it up: the
        // continuation lines attached to the message.
        let line = r#"[2026-09-09T10:00:00.090000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\ProductNotFound: "Product 42 not found" at /var/www/src/Controller/ProductController.php line 88 {"exception":"[object] (App\\Exception\\ProductNotFound(code: 0): Product 42 not found at /var/www/src/Controller/ProductController.php:88)"} {"token":"aaa"}"#;
        let mut entry = parse_line(line).expect("line valide");
        entry.message.push_str(
            "\n#0 /var/www/src/Controller/ProductController.php(88): App\\Repository\\ProductRepository->find(42)\n#1 {main}",
        );
        app.stats.ingest(0, entry);

        app.stats.finalize();
        app.on_event(Event::Tick);
        app
    }

    #[test]
    fn the_error_report_carries_the_signature_and_its_trace() {
        let mut app = app_with_one_error();
        app.tab = Tab::Errors;
        let report = report(&app);

        assert!(report.text.contains("ProductNotFound"), "the signature");
        assert!(report.text.contains("app_product_show"), "l'endpoint");
        assert!(report.text.contains("CRITICAL"), "the level");
        assert!(report.text.contains("var/log/prod.log"), "the source");
        // The screen shows three lines of it; the export, all of them.
        assert!(
            report.text.contains("#0 /var/www/src/Controller"),
            "the stack trace must be complete"
        );
        assert!(report.text.contains("#1 {main}"), "down to its last line");
        assert!(report.slug.starts_with("refrain-error-ProductNotFound-"));
    }

    #[test]
    fn the_deprecation_report_carries_its_origin() {
        let mut app = App::new(Cli::parse_from(["refrain", "var/log/prod.log"]), 1);
        // The deprecation names no route: it is the token shared with the
        // "Matched route" line that attaches it, while the request is open.
        let lines = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_product_show". {"route":"app_product_show"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.095000+02:00] php.INFO: User Deprecated: Since symfony/http-foundation 6.2: Calling "Request::getContentType()" is deprecated. {"exception":"[object] (ErrorException(code: 0): User Deprecated: Since symfony/http-foundation 6.2: Calling \"Request::getContentType()\" is deprecated. at /var/www/vendor/symfony/http-foundation/Request.php:1290)"} {"token":"aaa"}"#,
        ];
        for line in lines {
            app.stats.ingest(0, parse_line(line).expect("line valide"));
        }
        app.stats.finalize();
        app.on_event(Event::Tick);
        app.tab = Tab::Deprecations;
        let report = report(&app);

        assert!(report.text.contains("Request.php:1290"), "the origin");
        assert!(report.text.contains("app_product_show"), "the route");
        assert!(
            report.text.contains("getContentType"),
            "the message as written, not the folded key"
        );
        assert!(
            report.slug.starts_with("refrain-deprecation-Request-php-"),
            "{}",
            report.slug
        );
    }

    #[test]
    fn the_endpoint_report_carries_its_figures() {
        let mut app = app_with_one_error();
        app.tab = Tab::Endpoints;
        let report = report(&app);
        assert!(report.text.contains("app_product_show"));
        assert!(report.text.contains("120 ms"), "the measured duration");
        assert!(
            report
                .slug
                .starts_with("refrain-endpoint-app_product_show-")
        );
    }

    #[test]
    fn the_outbound_report_carries_the_shape_and_not_the_key() {
        let mut app = App::new(Cli::parse_from(["refrain", "var/log/prod.log"]), 1);
        let lines = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_checkout". {"route":"app_checkout"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.050000+02:00] http_client.INFO: Response: "429 https://api.payments.test/v2/charges?api_key=pk_live_4d2e8c" 1.250000 seconds {"http_method":"POST","http_code":429,"total_time":1.25} {"token":"aaa"}"#,
        ];
        for line in lines {
            app.stats.ingest(0, parse_line(line).expect("line valide"));
        }
        app.stats.finalize();
        app.on_event(Event::Tick);
        app.tab = Tab::Outbound;
        let report = report(&app);

        assert!(report.text.contains("POST api.payments.test/v2/charges"));
        assert!(report.text.contains("1.25 s"), "the latency");
        assert!(report.text.contains("1 × 4xx"), "the provider's answer");
        assert!(report.text.contains("app_checkout"), "who called it");
        // What `w` puts on disk and `y` on the clipboard is exactly where a
        // kept query string would have travelled furthest.
        assert!(!report.text.contains("pk_live"), "{}", report.text);
        assert!(
            report
                .slug
                .starts_with("refrain-outbound-POST-api-payments-test"),
            "{}",
            report.slug
        );
    }

    #[test]
    fn the_tabs_with_no_selection_return_the_summary() {
        let mut app = app_with_one_error();
        for tab in [Tab::Overview, Tab::Stream] {
            app.tab = tab;
            let report = report(&app);
            assert!(report.text.contains("summary"));
            assert!(report.text.contains("Top errors"));
        }
    }

    #[test]
    fn the_report_is_written_to_a_file() {
        let mut app = app_with_one_error();
        app.tab = Tab::Errors;

        let dir = std::env::temp_dir().join(format!("refrain-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_to(&app, &dir).expect("write");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("ProductNotFound"), "the signature");
        assert!(content.contains("#1 {main}"), "the trace, to the very end");

        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("refrain-error-ProductNotFound-"), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base64_suit_la_reference() {
        // The vectors from RFC 4648.
        for (clair, code) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(clair.as_bytes()), code, "base64({clair:?})");
        }
        // Non-ASCII: it is indeed the UTF-8 that gets encoded, byte by byte.
        assert_eq!(base64("é".as_bytes()), "w6k=");

        let sequence = clipboard_sequence("foobar");
        assert_eq!(sequence, "\x1b]52;c;Zm9vYmFy\x07");
    }

    #[test]
    fn the_file_name_stays_sane() {
        // An endpoint is often a URI, an exception a qualified name.
        assert_eq!(sanitize("/api/orders/42"), "api-orders-42");
        assert_eq!(sanitize("App\\Exception\\Boom"), "App-Exception-Boom");
        assert_eq!(sanitize("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize("///"), "");
        assert!(sanitize(&"a".repeat(100)).len() <= 40);
    }
}
