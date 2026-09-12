//! Turning a raw line into something usable: [`LogEntry`].
//!
//! Monolog writes two widespread formats, and we handle both:
//!
//! 1. **The line format** (`LineFormatter`, Symfony's default):
//!    `[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: Uncaught PHP Exception … {"exception":"…"} []`
//! 2. **The JSON format** (`JsonFormatter`): one line, one JSON object.
//!
//! Detection happens line by line rather than through an option: it is more
//! robust, and it allows following several files of different formats.

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use serde_json::Value;

/// Monolog's 8 severity levels (the PSR-3 standard), least to most severe.
///
/// Deriving `PartialOrd`/`Ord` on an enum uses **declaration order**:
/// `Level::Debug < Level::Error` is therefore true for free, which makes
/// filters like `entry.level >= min_level` trivial to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum)]
pub enum Level {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

impl Level {
    pub const ALL: [Level; 8] = [
        Level::Debug,
        Level::Info,
        Level::Notice,
        Level::Warning,
        Level::Error,
        Level::Critical,
        Level::Alert,
        Level::Emergency,
    ];

    /// From the textual name of the line format (`request.CRITICAL:` → `CRITICAL`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "NOTICE" => Self::Notice,
            "WARNING" => Self::Warning,
            "ERROR" => Self::Error,
            "CRITICAL" => Self::Critical,
            "ALERT" => Self::Alert,
            "EMERGENCY" => Self::Emergency,
            _ => return None,
        })
    }

    /// From the numeric PSR-3 code of the `JsonFormatter` (100 = DEBUG … 600 = EMERGENCY).
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            100 => Self::Debug,
            200 => Self::Info,
            250 => Self::Notice,
            300 => Self::Warning,
            400 => Self::Error,
            500 => Self::Critical,
            550 => Self::Alert,
            600 => Self::Emergency,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Notice => "NOTICE",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
            Self::Alert => "ALERT",
            Self::Emergency => "EMERGENCY",
        }
    }

    /// Lowercase name, as it appears in the JSON output.
    pub fn lower(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
            Self::Alert => "alert",
            Self::Emergency => "emergency",
        }
    }

    /// Short form, to fit in a table column.
    pub fn short(self) -> &'static str {
        match self {
            Self::Debug => "DEBG",
            Self::Info => "INFO",
            Self::Notice => "NOTC",
            Self::Warning => "WARN",
            Self::Error => "ERRO",
            Self::Critical => "CRIT",
            Self::Alert => "ALRT",
            Self::Emergency => "EMER",
        }
    }

    /// `self as usize`: a data-less enum is represented by its rank. Handy for
    /// indexing a `[u64; 8]` of counters.
    pub fn index(self) -> usize {
        self as usize
    }

    /// ERROR and above: what counts as an "error" in the statistics.
    pub fn is_error(self) -> bool {
        self >= Level::Error
    }
}

/// A parsed log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: Option<DateTime<FixedOffset>>,
    pub level: Level,
    pub channel: String,
    pub message: String,
    pub context: Option<Value>,
    pub extra: Option<Value>,
}

impl LogEntry {
    /// Looks a key up in `context` first, then in `extra`.
    ///
    /// The `'a` lifetime tells the compiler: "the returned `Value` lives as
    /// long as the entry". That is what allows returning a reference into
    /// `self` without copying anything.
    pub fn lookup<'a>(&'a self, key: &str) -> Option<&'a Value> {
        self.context
            .as_ref()
            .and_then(|c| c.get(key))
            .or_else(|| self.extra.as_ref().and_then(|e| e.get(key)))
            .filter(|v| !v.is_null())
    }

    /// Symfony route name, if present.
    ///
    /// Symfony logs it through the `request` channel ("Matched route
    /// \"app_x\".") in `context.route`, and duplicates it in
    /// `context.route_parameters._route`.
    pub fn route(&self) -> Option<&str> {
        if let Some(v) = self.lookup("route").and_then(Value::as_str) {
            return Some(v);
        }
        self.context
            .as_ref()?
            .get("route_parameters")?
            .get("_route")?
            .as_str()
    }

    pub fn request_uri(&self) -> Option<&str> {
        self.lookup("request_uri")
            .or_else(|| self.lookup("uri"))
            .or_else(|| self.lookup("url"))
            .and_then(Value::as_str)
    }

    pub fn method(&self) -> Option<&str> {
        self.lookup("method").and_then(Value::as_str)
    }

    /// HTTP status of the response, if it is logged.
    ///
    /// Monolog writes none of its own: it is the `kernel.terminate` subscriber
    /// that puts it in the context (see the README). The key varies from one
    /// application to the next, and some formatters render the code as a
    /// string — we accept both rather than adding an option.
    pub fn status(&self) -> Option<u16> {
        let value = self
            .lookup("status")
            .or_else(|| self.lookup("status_code"))
            .or_else(|| self.lookup("http_status"))
            .or_else(|| self.lookup("response_code"))?;
        let code = match value {
            Value::Number(n) => n.as_i64()?,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        // Outside the HTTP range it is not a status: a "status" of 0 or 9999
        // comes from another field of the same name.
        (100..600).contains(&code).then_some(code as u16)
    }

    /// The label requests are grouped under: the route if there is one,
    /// otherwise the URI stripped of its query string.
    pub fn endpoint(&self) -> Option<String> {
        if let Some(r) = self.route() {
            return Some(r.to_string());
        }
        let uri = self.request_uri()?;
        let path = uri.split('?').next().unwrap_or(uri);
        Some(path.to_string())
    }

    /// Exception class, taken from `context.exception` or, failing that, from
    /// the message.
    ///
    /// Monolog serialises the exception in two ways depending on the formatter:
    /// - string: `[object] (App\Exception\Foo(code: 0): msg at /src/X.php:88)`
    /// - object: `{"class":"App\\Exception\\Foo","message":"…"}`
    pub fn exception_class(&self) -> Option<&str> {
        let exc = self.lookup("exception")?;

        if let Some(class) = exc.get("class").and_then(Value::as_str) {
            return Some(class);
        }
        if let Some(s) = exc.as_str() {
            return class_from_object_string(s);
        }
        None
    }

    /// A stable key to group "the same error" seen N times.
    ///
    /// The message is normalised (digits and quoted strings replaced) so that
    /// "Product 42 not found" and "Product 1337 not found" count as one and the
    /// same error.
    pub fn signature(&self) -> String {
        let mut sig = String::with_capacity(96);
        if let Some(class) = self.exception_class() {
            sig.push_str(short_class(class));
            sig.push_str(": ");
        }
        // The first line only: the stack trace attached to it would make two
        // distinct groups of the same error depending on whether it is there.
        let head = self.message.lines().next().unwrap_or(&self.message);
        normalize_into(head, &mut sig);
        truncate_chars(&mut sig, 160);
        sig
    }
}

/// `App\Exception\ProductNotFound` → `ProductNotFound`
pub fn short_class(class: &str) -> &str {
    class.rsplit('\\').next().unwrap_or(class)
}

/// Extracts `App\Exception\Foo` from `[object] (App\Exception\Foo(code: 0): …)`.
fn class_from_object_string(s: &str) -> Option<&str> {
    let after_paren = &s[s.find('(')? + 1..];
    // The class stops at the parenthesis of `(code: 0)`, or at the `:` if there is none.
    let end = after_paren
        .find('(')
        .or_else(|| after_paren.find(':'))
        .unwrap_or(after_paren.len());
    let class = after_paren[..end].trim();
    (!class.is_empty()).then_some(class)
}

/// Writes `src` into `dst`, erasing everything that varies between occurrences.
fn normalize_into(src: &str, dst: &mut String) {
    let mut chars = src.chars().peekable();
    let mut last_was_digit = false;

    while let Some(c) = chars.next() {
        match c {
            // A quoted string is almost always a varying value (an id, a file
            // name, a route): replace it wholesale.
            '"' => {
                dst.push_str("\"…\"");
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                }
                last_was_digit = false;
            }
            // A run of digits becomes a single `#`.
            '0'..='9' => {
                if !last_was_digit {
                    dst.push('#');
                    last_was_digit = true;
                }
            }
            _ => {
                dst.push(c);
                last_was_digit = false;
            }
        }
    }
}

/// Truncates on character boundaries (a Rust `String` is UTF-8: cutting at an
/// arbitrary byte index would panic).
pub fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push('…');
    }
}

/// Entry point: a raw line → an entry, or `None` if the line is not the start
/// of an entry (an empty line, or the continuation of a stack trace).
pub fn parse_line(line: &str) -> Option<LogEntry> {
    let line = line.trim_end_matches(['\n', '\r']);
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        parse_json(trimmed)
    } else if trimmed.starts_with('[') {
        parse_text(trimmed)
    } else {
        None
    }
}

/// `JsonFormatter` format: one JSON object per line.
fn parse_json(line: &str) -> Option<LogEntry> {
    let mut v: Value = serde_json::from_str(line).ok()?;

    // The level arrives in two shapes: `level_name` ("ERROR") or `level` (400).
    let level = v
        .get("level_name")
        .and_then(Value::as_str)
        .and_then(Level::from_name)
        .or_else(|| {
            v.get("level")
                .and_then(Value::as_i64)
                .and_then(Level::from_code)
        })?;

    let channel = v
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("app")
        .to_string();

    let message = v
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ts = v.get("datetime").and_then(|d| match d {
        Value::String(s) => parse_ts(s),
        // Some versions serialise PHP's whole DateTime object.
        Value::Object(_) => d.get("date").and_then(Value::as_str).and_then(parse_ts),
        _ => None,
    });

    // `take()` avoids cloning the JSON tree: we move it out of `v`, which dies
    // at the end of the function anyway.
    let context = v.get_mut("context").map(Value::take).filter(is_useful);
    let extra = v.get_mut("extra").map(Value::take).filter(is_useful);

    Some(LogEntry {
        ts,
        level,
        channel,
        message,
        context,
        extra,
    })
}

/// `LineFormatter` format: `[date] channel.LEVEL: message {context} {extra}`
fn parse_text(line: &str) -> Option<LogEntry> {
    let close = line.find(']')?;
    let ts = parse_ts(&line[1..close]);

    let rest = line[close + 1..].trim_start();

    // `channel.LEVEL:` — the first `:` ends the header. The message that
    // follows can contain any number of them, hence looking for the first only.
    let colon = rest.find(':')?;
    let (channel, level) = rest[..colon].rsplit_once('.')?;
    let level = Level::from_name(level.trim())?;

    let body = rest[colon + 1..].trim_start();
    let (message, context, extra) = split_message_and_json(body);

    Some(LogEntry {
        ts,
        level,
        channel: channel.trim().to_string(),
        message: message.to_string(),
        context: context.filter(is_useful),
        extra: extra.filter(is_useful),
    })
}

/// Splits `message {context} {extra}` into its three pieces.
///
/// The difficulty: the message itself may contain braces. So we look for the
/// **leftmost** cut such that everything after it is a run of valid JSON values
/// consuming the end of the line.
fn split_message_and_json(body: &str) -> (&str, Option<Value>, Option<Value>) {
    for (i, w) in body.as_bytes().windows(2).enumerate() {
        // Only plausible positions are tested: a space followed by `{` or `[`.
        if w[0] != b' ' || (w[1] != b'{' && w[1] != b'[') {
            continue;
        }
        // `i` points at an ASCII space: cutting there is UTF-8 safe.
        if let Some((context, extra)) = parse_trailing_values(&body[i + 1..]) {
            return (body[..i].trim_end(), context, extra);
        }
    }
    (body.trim_end(), None, None)
}

/// True if `tail` is exactly 1 or 2 JSON values, and nothing else.
fn parse_trailing_values(tail: &str) -> Option<(Option<Value>, Option<Value>)> {
    let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
    let mut values = Vec::with_capacity(2);

    for value in stream.by_ref() {
        values.push(value.ok()?);
        if values.len() > 2 {
            return None;
        }
    }
    // The whole end of the line must have been consumed; otherwise the `{` we
    // found was part of the message and not of the context.
    if values.is_empty() || !tail[stream.byte_offset()..].trim().is_empty() {
        return None;
    }

    let mut it = values.into_iter();
    Some((it.next(), it.next()))
}

/// Monolog writes `[]` / `{}` when context or extra are empty: may as well forget them.
fn is_useful(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => !m.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Accepts the two dates met in practice in Symfony logs: RFC 3339
/// (`2026-09-09T10:23:45.123456+02:00`) and the older zone-less format.
fn parse_ts(s: &str) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            // No zone in the line: the machine's is assumed.
            if let Some(dt) = Local.from_local_datetime(&naive).single() {
                return Some(dt.fixed_offset());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symfony_line_yields_its_context() {
        let line = r#"[2026-09-09T10:23:45.123456+02:00] request.INFO: Matched route "app_product_show". {"route":"app_product_show","route_parameters":{"_route":"app_product_show","id":"42"},"request_uri":"https://ex.test/product/42","method":"GET"} []"#;
        let e = parse_line(line).expect("the line must be recognised");

        assert_eq!(e.level, Level::Info);
        assert_eq!(e.channel, "request");
        assert_eq!(e.message, r#"Matched route "app_product_show"."#);
        assert_eq!(e.route(), Some("app_product_show"));
        assert_eq!(e.method(), Some("GET"));
        assert!(e.ts.is_some());
        // A trailing `[]` is an empty extra: we do not keep it.
        assert!(e.extra.is_none());
    }

    #[test]
    fn the_status_is_read_under_its_usual_names() {
        let with_context = |context: &str| {
            let line = format!(
                r#"[2026-09-09T10:23:45+02:00] request.INFO: Request finished {context} []"#
            );
            parse_line(&line).expect("line valide").status()
        };

        assert_eq!(with_context(r#"{"status":500}"#), Some(500));
        assert_eq!(with_context(r#"{"status_code":404}"#), Some(404));
        assert_eq!(with_context(r#"{"http_status":201}"#), Some(201));
        assert_eq!(with_context(r#"{"response_code":302}"#), Some(302));
        // Some formatters render the code as a string.
        assert_eq!(with_context(r#"{"status":"200"}"#), Some(200));

        // Outside the HTTP range, it is another field of the same name: an
        // application status, a flag, a home-grown error code.
        assert_eq!(with_context(r#"{"status":0}"#), None);
        assert_eq!(with_context(r#"{"status":9999}"#), None);
        assert_eq!(with_context(r#"{"status":"ok"}"#), None);
        assert_eq!(with_context("{}"), None);
    }

    #[test]
    fn an_exception_and_the_signature_it_groups_under() {
        let line = r#"[2026-09-09T10:23:46+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\ProductNotFound: "Product 42 not found" at /var/www/src/X.php line 88 {"exception":"[object] (App\\Exception\\ProductNotFound(code: 0): Product 42 not found at /var/www/src/X.php:88)"} []"#;
        let e = parse_line(line).unwrap();

        assert_eq!(e.level, Level::Critical);
        assert_eq!(e.exception_class(), Some(r"App\Exception\ProductNotFound"));

        // Two different ids must produce the same signature.
        let other = line.replace("42", "1337");
        let e2 = parse_line(&other).unwrap();
        assert_eq!(e.signature(), e2.signature());
        assert!(e.signature().starts_with("ProductNotFound: "));
    }

    #[test]
    fn a_message_containing_braces_is_not_cut_there() {
        // The `{` in the message must not be taken for the start of the context.
        let line = r#"[2026-09-09T10:23:45+02:00] app.WARNING: Template {name} is deprecated {"name":"old.twig"} []"#;
        let e = parse_line(line).unwrap();
        assert_eq!(e.message, "Template {name} is deprecated");
        assert_eq!(e.lookup("name").and_then(|v| v.as_str()), Some("old.twig"));
    }

    #[test]
    fn the_json_formatter_is_recognised_too() {
        let line = r#"{"message":"Boom","context":{"duration_ms":123.5},"level":500,"level_name":"CRITICAL","channel":"app","datetime":"2026-09-09T10:23:45.000000+02:00","extra":{}}"#;
        let e = parse_line(line).unwrap();

        assert_eq!(e.level, Level::Critical);
        assert_eq!(e.channel, "app");
        assert_eq!(
            e.lookup("duration_ms").and_then(|v| v.as_f64()),
            Some(123.5)
        );
        assert!(e.extra.is_none());
    }

    #[test]
    fn a_continuation_line_is_not_an_entry() {
        // A stack trace line starts with neither `[` nor `{`.
        assert!(parse_line("  #0 /var/www/src/X.php(88): App\\Foo->bar()").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn levels_compare_by_severity() {
        assert!(Level::Debug < Level::Error);
        assert!(Level::Critical.is_error());
        assert!(!Level::Warning.is_error());
    }
}

#[cfg(test)]
mod robustness {
    use super::*;

    /// A tiny deterministic pseudo-random generator: the same seed replays
    /// exactly the same twisted lines, with no dependency and no flaky test.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const TEMPLATES: [&str; 6] = [
        r#"[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\Exception\Boom(code: 0): nope)","route":"app_home"} {"token":"aaa"}"#,
        r#"{"message":"Matched route","context":{"route":"app_home","duration_ms":12.5},"level":200,"channel":"request","datetime":"2026-09-09T10:23:45.123456+02:00"}"#,
        r#"[2026-09-09T10:23:45.123456+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT t0.id FROM produit t0 WHERE t0.id = ?","params":{"1":42}} []"#,
        "#0 /var/www/src/Controller/ProductController.php(88): App\\Repository->find(42)",
        "",
        "{",
    ];

    /// The parser swallows text nobody controls: lines truncated by a
    /// rotation, JSON cut in half, unbalanced braces. It may return `None` —
    /// the line is then counted as skipped — but it must not bring down a
    /// dashboard that has been running for three days.
    ///
    /// Invalid UTF-8 is not exercised here: `tail.rs` converts it beforehand,
    /// and the parser only ever sees valid `&str`.
    #[test]
    fn no_twisted_line_makes_the_parser_panic() {
        // 1. Every possible truncation, at each character boundary.
        for template in TEMPLATES {
            for (index, _) in template.char_indices() {
                exercise(&template[..index]);
            }
            exercise(template);
        }

        // 2. Twenty thousand fixed-seed mutations, using the very characters
        //    that make up the structure of a Monolog line.
        const POISON: [char; 14] = [
            '"', '{', '}', '[', ']', '\\', ':', ',', '\0', '\n', '\t', 'é', '日', '🙂',
        ];
        let mut rng = Xorshift(0x5eed_1234_abcd);
        for _ in 0..20_000 {
            let template = TEMPLATES[rng.below(TEMPLATES.len())];
            let mut chars: Vec<char> = template.chars().collect();
            if chars.is_empty() {
                continue;
            }
            for _ in 0..1 + rng.below(3) {
                let position = rng.below(chars.len());
                chars[position] = POISON[rng.below(POISON.len())];
            }
            exercise(&chars.into_iter().collect::<String>());
        }

        // 3. The absurdities one would write by hand.
        for text in [
            "[",
            "]",
            "{}",
            "[]",
            "[2026",
            "{\"",
            "{\"a\":",
            "[] {} []",
            "[2026-09-09T10:23:45.123456+02:00]",
            "[2026-09-09T10:23:45+02:00] a.B:",
            "[9999999999999-99-99T99:99:99.999999+99:99] a.INFO: x {} []",
            "{\"level\":999999999999999999999}",
            "{\"datetime\":[]}",
        ] {
            exercise(text);
        }
    }

    /// Parses the line and, if it yields an entry, exercises everything drawn
    /// from it afterwards: that is where the string slicing hides.
    fn exercise(line: &str) {
        let Some(entry) = parse_line(line) else {
            return;
        };
        let _ = entry.signature();
        let _ = entry.endpoint();
        let _ = entry.exception_class();
        let _ = entry.route();
        let _ = entry.request_uri();
        let _ = entry.method();
        let mut copy = entry.message.clone();
        truncate_chars(&mut copy, 7);
        assert!(copy.chars().count() <= 8, "truncation stays bounded");
    }

    #[test]
    fn truncation_respects_multi_byte_characters() {
        // Cutting on a byte would panic in the middle of a character; the
        // ellipsis added counts for one more character.
        for text in ["ééééééééé", "日本語日本語日本語", "🙂🙂🙂🙂🙂", "abc"]
        {
            for limit in 0..12 {
                let mut copy = text.to_string();
                truncate_chars(&mut copy, limit);
                assert!(copy.chars().count() <= limit + 1, "{text} at {limit}");
            }
        }
    }
}
