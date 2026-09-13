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

/// The channel Symfony's HttpClient logs its outbound calls on.
const HTTP_CLIENT_CHANNEL: &str = "http_client";

/// One outbound HTTP call, as a `http_client` line describes it.
///
/// `target` never carries a query string. That is not a simplification: a
/// third party's URL is where an API key sits, and a key kept in the grouping
/// key would end up in the table, in the JSON and in an export. It is dropped
/// whole rather than folded, because folding leaves what it did not recognise.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpCall {
    /// The verb, when the line names one.
    pub method: Option<String>,
    /// Host and path, credentials and query string gone, identifiers folded.
    pub target: String,
    /// The status the third party answered — a 429 the request's own status
    /// never tells.
    pub status: Option<u16>,
    /// `total_time`, in seconds: curl's unit.
    pub seconds: Option<f64>,
}

impl HttpCall {
    /// What the call is grouped and displayed under: the verb, then the host
    /// and the path. Without a verb the host and the path are the whole key —
    /// saying "GET" where the line said nothing would be an invention.
    pub fn shape(&self) -> String {
        match &self.method {
            Some(method) => format!("{method} {}", self.target),
            None => self.target.clone(),
        }
    }
}

/// The channel Symfony Messenger logs on. An audit middleware conventionally
/// gives itself `messenger_audit`, so the prefix is what is matched.
const MESSENGER_CHANNEL: &str = "messenger";

/// What a Messenger line says happened to a message.
///
/// Three of these come from the queue and one does not: `Ran` is a handler
/// being invoked, which happens for a synchronous dispatch too, where there
/// is no queue and therefore no backlog to speak of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEvent {
    /// Handed to a transport. A message routed to two senders is dispatched
    /// twice, because it really is sent twice.
    Dispatched,
    /// A worker acknowledged it to the transport: the message is off the
    /// queue. Counted here and not on `Message … handled by …`, which fires
    /// once per handler and would count a two-handler message twice.
    Handled,
    /// A handler ran. Informational beside `Handled`: it is the only thing a
    /// synchronous dispatch writes.
    Ran,
    /// Nothing was registered to handle it.
    NoHandler,
    /// It threw and goes back for another attempt.
    Retried,
    /// It threw for the last time: removed from the transport, or rejected to
    /// the failure transport.
    Failed,
}

/// One Messenger line, read.
#[derive(Debug, Clone, PartialEq)]
pub struct MessengerLine {
    pub event: MessageEvent,
    /// The message class, fully qualified: `App\Message\IndexEntityMessage`.
    pub class: String,
    /// The identifier that pairs a dispatch with its handling, when one is
    /// logged. Core Symfony writes none on the dispatch side — see
    /// [`LogEntry::messenger`].
    pub id: Option<String>,
    /// Written by an audit middleware rather than by Symfony's own templates.
    ///
    /// It matters because an application running both writes **two** lines for
    /// one dispatch, and the two must not be added together.
    pub audited: bool,
}

/// The channel Symfony's cache adapters log on.
const CACHE_CHANNEL: &str = "cache";

/// What a cache line says happened to an item.
///
/// Symfony writes a line when it **computes** an item and nothing at all when
/// it serves one from the cache. Every line here is therefore a miss; what
/// differs is who paid for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEvent {
    /// This process computed the item: the miss that cost something.
    Computed,
    /// The item was already being computed elsewhere and this one waited.
    /// The lock exists to blunt a stampede, and this line is it happening.
    Contended,
}

/// One cache line, read.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheLine {
    pub event: CacheEvent,
    /// The item's key, as the log gives it.
    pub key: String,
}

/// The channel Symfony's console logs on.
const CONSOLE_CHANNEL: &str = "console";

/// One console line, read.
///
/// A command is the cron job's endpoint: it has a name, an exit code that is
/// a status, and — through the token its process shares — a duration.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandLine {
    /// The command, without its arguments: `app:import`.
    pub name: String,
    /// The exit code, on the line that says the run ended. Exactly one line
    /// per run carries it, which is what makes it the run counter.
    pub code: Option<i64>,
    /// This line reports an exception thrown while the command ran.
    pub threw: bool,
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
        // On an outbound call, `url` is the third party's address, not ours:
        // it names no endpoint of this application, and it is the one place a
        // provider's API key sits in plain sight. Reading it here would put
        // that key in the endpoint table, in the JSON and in an export.
        if self.is_http_client() {
            return None;
        }
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
        // An outbound call's status is the third party's answer, not this
        // application's: counting it among our responses would make a
        // provider's 429 look like a 4xx we served.
        if self.is_http_client() {
            return None;
        }
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

    /// Does this line come from Symfony's HttpClient?
    pub fn is_http_client(&self) -> bool {
        self.channel.eq_ignore_ascii_case(HTTP_CLIENT_CHANNEL)
    }

    /// The outbound call this line reports, if it reports one.
    ///
    /// Symfony writes two lines per call — the announcement, then the
    /// response — and only the second carries the status and the duration.
    /// Counting both would double every call, so only the response is one.
    pub fn http_call(&self) -> Option<HttpCall> {
        if !self.is_http_client() {
            return None;
        }
        let head = self.message.lines().next().unwrap_or(&self.message);
        if head.trim_start().starts_with("Request:") {
            return None;
        }

        let (status, url, seconds) = response_message(head);
        // The context wins over the message: it holds the values as curl
        // measured them, where the message holds what was formatted for a
        // human.
        let url = self
            .lookup("url")
            .and_then(Value::as_str)
            .or(url)
            .filter(|u| !u.trim().is_empty())?;

        Some(HttpCall {
            method: self.call_method(),
            target: http_target(url),
            status: self
                .lookup("http_code")
                .or_else(|| self.lookup("status_code"))
                .or_else(|| self.lookup("status"))
                .and_then(status_code)
                .or(status),
            // `total_time` is curl's, and curl measures in seconds: there is
            // no unit to infer here, unlike a duration field of the
            // application's own choosing.
            seconds: self
                .lookup("total_time")
                .and_then(non_negative_number)
                .or(seconds),
        })
    }

    /// The verb of an outbound call. Symfony's HttpClient carries it in the
    /// info array it hands to `getInfo()`, under `http_method`; an application
    /// logging its own context usually calls it `method`.
    fn call_method(&self) -> Option<String> {
        let raw = self
            .lookup("http_method")
            .or_else(|| self.lookup("method"))
            .and_then(Value::as_str)?
            .trim();
        // A verb is a short word: anything else is another field of the same
        // name, and would become a key of its own in the table.
        (!raw.is_empty() && raw.len() <= 16 && raw.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| raw.to_ascii_uppercase())
    }

    /// Does this line come from Symfony Messenger?
    pub fn is_messenger(&self) -> bool {
        // Compared as bytes: a mangled line can carry anything as a channel,
        // and cutting a `String` at a byte index that falls inside a character
        // would panic — which the twisted-line test found the moment it ran.
        self.channel
            .as_bytes()
            .get(..MESSENGER_CHANNEL.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(MESSENGER_CHANNEL.as_bytes()))
    }

    /// What this line says happened to a message, if it says anything.
    ///
    /// Two vocabularies are read, and an application may write both.
    ///
    /// **Symfony's own**, on the `messenger` channel. The templates are
    /// matched on the part that does not vary, never on the `{class}`
    /// placeholder: Monolog interpolates it only when `PsrLogMessageProcessor`
    /// is configured, so the same event reaches us with or without it.
    ///
    /// **An audit middleware's** — `[id] Sent App\Message\Foo` — which is
    /// not core Symfony but the widespread pattern, and the only thing that
    /// gives a dispatch an identifier: core writes `message_id` on the worker
    /// side only, so without this middleware a dispatch cannot be paired with
    /// its handling and the lag stays unknown.
    pub fn messenger(&self) -> Option<MessengerLine> {
        if !self.is_messenger() {
            return None;
        }
        let head = self.message.lines().next().unwrap_or(&self.message).trim();

        if let Some(audit) = audit_line(head) {
            let (id, event, class) = audit;
            return Some(MessengerLine {
                event,
                class: self.message_class().unwrap_or(class).to_string(),
                id: Some(id.to_string()),
                audited: true,
            });
        }

        let event = symfony_event(head)?;
        Some(MessengerLine {
            event,
            class: self
                .message_class()
                .or_else(|| class_from_template(head, event))?
                .to_string(),
            // Written by the worker on the lines it produces: the
            // acknowledgement, the retry and the final failure.
            id: self.lookup("message_id").and_then(value_as_id),
            audited: false,
        })
    }

    /// The message class as the context names it. Core Messenger puts it in
    /// `class` on every line it writes.
    fn message_class(&self) -> Option<&str> {
        self.lookup("class")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|class| !class.is_empty())
    }

    /// The cache miss this line reports, if it reports one.
    ///
    /// Symfony's `LockRegistry` writes `Lock acquired, now computing item
    /// "{key}"` when a process computes an item, and nothing when one is
    /// served from the cache — so a key that appears on every request is a
    /// cache that is not working, and this is the only place it shows.
    ///
    /// Matched on the fixed part of each template, never on `{key}`: Monolog
    /// interpolates it only when `PsrLogMessageProcessor` is configured, and
    /// the key itself comes from `context.key`, which is always there.
    pub fn cache_miss(&self) -> Option<CacheLine> {
        if !self.channel.eq_ignore_ascii_case(CACHE_CHANNEL) {
            return None;
        }
        let head = self.message.lines().next().unwrap_or(&self.message);
        let event = if head.contains("now computing item ") {
            CacheEvent::Computed
        } else if head.contains("is locked, waiting for it to be released") {
            // The two lines that follow a wait — "retrieved after lock was
            // released", "not found … now retrying" — are its outcome, not a
            // second miss, and counting them would double the contention.
            CacheEvent::Contended
        } else {
            return None;
        };

        let key = match self.lookup("key").and_then(Value::as_str) {
            Some(key) => key.trim(),
            None => quoted_value(head)?,
        };
        // `{key}` left as written by a formatter with no PSR processor is not
        // a key: one row named `{key}` would be a lie.
        (!key.is_empty() && !key.starts_with('{')).then(|| CacheLine {
            event,
            key: key.to_string(),
        })
    }

    /// The console command this line reports on, if it names one.
    ///
    /// Symfony writes nothing when a command **starts**, one line when it
    /// ends — `Command "{command}" exited with code "{code}"`, at DEBUG — and
    /// one more if it threw. The first of those is the run counter: exactly
    /// one per run.
    ///
    /// Lines that name no command (`The console exited with code "{code}"`,
    /// written when the input resolved to none) are left alone: there is
    /// nothing to group them under, and a row named for the console itself
    /// would answer no question.
    pub fn command(&self) -> Option<CommandLine> {
        if !self.channel.eq_ignore_ascii_case(CONSOLE_CHANNEL) {
            return None;
        }
        let head = self.message.lines().next().unwrap_or(&self.message);
        let exited = head.starts_with("Command ") && head.contains(" exited with code ");
        let threw = head.starts_with("Error thrown while running command ");
        if !exited && !threw {
            return None;
        }

        let name = match self.lookup("command").and_then(Value::as_str) {
            Some(name) => name,
            None => quoted_value(head)?,
        };
        // The context sometimes carries the whole command line, arguments
        // included: `app:import --env=prod` is the same command as
        // `app:import --env=dev`, and one row per invocation would answer
        // nothing.
        let name = name.split_whitespace().next()?;
        // `{command}` left as written by a formatter with no PSR processor is
        // not a command name.
        if name.is_empty() || name.starts_with('{') {
            return None;
        }

        Some(CommandLine {
            name: name.to_string(),
            code: exited.then(|| self.exit_code(head)).flatten(),
            threw,
        })
    }

    /// The exit code, from the context or from the message's last quoted run.
    fn exit_code(&self, head: &str) -> Option<i64> {
        if let Some(value) = self.lookup("code") {
            return match value {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            };
        }
        // `Command "app:import" exited with code "1"`: the code is the last
        // quoted value, the name being the first.
        let (_, tail) = head.rsplit_once("exited with code ")?;
        tail.trim().trim_matches('"').parse().ok()
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

    /// Where the exception was raised, as `path:line`, when the formatter
    /// wrote it.
    ///
    /// The two serialisations again: the object form carries a `file` field
    /// already joined with the line; the string form ends with
    /// `at /path/File.php:42)`. The message may itself contain " at ", so the
    /// **last** one is the right one.
    pub fn exception_origin(&self) -> Option<&str> {
        let exc = self.lookup("exception")?;
        if let Some(file) = exc.get("file").and_then(Value::as_str) {
            return Some(file);
        }
        origin_from_object_string(exc.as_str()?)
    }

    /// A deprecation, as Symfony's `ErrorHandler` logs it: the message is
    /// prefixed with the PHP error level's name — `User Deprecated: ` for
    /// `trigger_deprecation()`, `Deprecated: ` for the engine's own. The
    /// channel is not looked at: it is `php` by default, `deprecation` when
    /// the Monolog recipe's dedicated handler is configured.
    pub fn is_deprecation(&self) -> bool {
        deprecation_body(&self.message).is_some()
    }

    /// The key deprecations group under: the message and its origin, both
    /// normalised the way an error signature is.
    ///
    /// The route is deliberately **not** in it: one deprecated call reached
    /// from twenty routes is one thing to fix, not twenty. The origin, on the
    /// other hand, is: for a `trigger_deprecation()` the `ErrorHandler`
    /// points it at the deprecated code itself, which is what tells
    /// `The "…" class is deprecated` for two different classes apart once
    /// the normaliser has erased their names. Its line number is normalised
    /// with the rest, so a deployment in the middle of the file does not
    /// split a row in two.
    pub fn deprecation_key(&self) -> (String, String) {
        let body = deprecation_body(&self.message).unwrap_or(&self.message);
        let head = body.lines().next().unwrap_or(body);
        let mut message = String::with_capacity(head.len().min(200));
        normalize_into(head, &mut message);
        truncate_chars(&mut message, 200);

        let mut origin = String::new();
        if let Some(path) = self.exception_origin() {
            normalize_into(path, &mut origin);
            truncate_chars(&mut origin, 200);
        }
        (message, origin)
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

/// The first double-quoted run of a message: `item "homepage_teasers"` →
/// `homepage_teasers`. What is left to read when the context is gone.
fn quoted_value(head: &str) -> Option<&str> {
    let (_, rest) = head.split_once('"')?;
    let (inside, _) = rest.split_once('"')?;
    Some(inside)
}

/// Classifies a line against Symfony Messenger's own templates.
///
/// Matched on the fixed head of each template — `Sending message ` and not
/// `Sending message {class}` — because Monolog leaves the placeholder in place
/// unless `PsrLogMessageProcessor` is configured, and both forms must read the
/// same.
fn symfony_event(head: &str) -> Option<MessageEvent> {
    // `Error thrown while handling message X. Sending for retry #2 …` starts
    // like the final failure and means the opposite, so the tail decides.
    if head.starts_with("Error thrown while handling message ") {
        return Some(match head.contains("Removing from transport") {
            true => MessageEvent::Failed,
            false => MessageEvent::Retried,
        });
    }
    if head.starts_with("Sending message ") {
        return Some(MessageEvent::Dispatched);
    }
    if head.starts_with("Rejected message ") {
        return Some(MessageEvent::Failed);
    }
    if head.starts_with("No handler for message ") {
        return Some(MessageEvent::NoHandler);
    }
    if head.contains("was handled successfully") {
        return Some(MessageEvent::Handled);
    }
    if head.starts_with("Message ") && head.contains(" handled by ") {
        return Some(MessageEvent::Ran);
    }
    None
}

/// The class a template names, for the lines whose context does not carry one.
///
/// Only where the class is the first thing after a fixed prefix: elsewhere —
/// `Message X handled by Y` — the context is the only reliable source, and an
/// uninterpolated `{class}` is no class at all.
fn class_from_template(head: &str, event: MessageEvent) -> Option<&str> {
    let rest = match event {
        MessageEvent::Dispatched => head.strip_prefix("Sending message ")?,
        MessageEvent::NoHandler => head.strip_prefix("No handler for message ")?,
        MessageEvent::Handled => head,
        MessageEvent::Failed | MessageEvent::Retried => head
            .strip_prefix("Error thrown while handling message ")
            .or_else(|| head.strip_prefix("Rejected message "))?,
        MessageEvent::Ran => return None,
    };
    let class = rest.split_whitespace().next()?.trim_end_matches('.');
    // `{class}` left as written by a formatter with no PSR processor: the
    // placeholder is not a class, and one row named `{class}` would be a lie.
    (!class.is_empty() && !class.starts_with('{')).then_some(class)
}

/// `[1a2b3c] Sent App\Message\Foo` → `("1a2b3c", Dispatched, "App\Message\Foo")`.
///
/// The shape an audit middleware writes. `Sent` and `Received` are its whole
/// vocabulary; anything else between the brackets is somebody else's line and
/// is left to [`symfony_event`].
fn audit_line(head: &str) -> Option<(&str, MessageEvent, &str)> {
    let rest = head.strip_prefix('[')?;
    let (id, rest) = rest.split_once(']')?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let mut words = rest.split_whitespace();
    let event = match words.next()? {
        "Sent" => MessageEvent::Dispatched,
        "Received" => MessageEvent::Handled,
        _ => return None,
    };
    let class = words.next()?;
    (!class.is_empty() && !class.starts_with('{')).then_some((id, event, class))
}

/// A transport names its messages with a string or with a number — AMQP's
/// delivery tag is an integer — and both are identifiers.
fn value_as_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Reads what Symfony's HttpClient writes on the response:
/// `Response: "200 https://api.example.com/v1/geocode?q=…" 0.214782 seconds`
/// → `(Some(200), Some("https://…"), Some(0.214782))`.
///
/// Everything is optional: the bare formatter writes only the quoted part, and
/// an application may reformat the line altogether — in which case the context
/// is what is left to read.
fn response_message(head: &str) -> (Option<u16>, Option<&str>, Option<f64>) {
    let Some(open) = head.find('"') else {
        return (None, None, None);
    };
    let rest = &head[open + 1..];
    let Some(close) = rest.rfind('"') else {
        return (None, None, None);
    };
    let (inside, tail) = (rest[..close].trim(), &rest[close + 1..]);

    // `200 https://…`: the status, then the URL. A first word that is not a
    // status means the whole of it is the URL — nothing is guessed from it.
    let (status, url) = match inside.split_once(char::is_whitespace) {
        Some((first, rest)) => match first.parse::<u16>().ok().filter(is_http_status) {
            Some(code) => (Some(code), rest.trim()),
            None => (None, inside),
        },
        None => (None, inside),
    };

    // `… " 0.214782 seconds`. The unit is required: a bare number after the
    // quote is something else, and reading it as a duration would invent one.
    let mut words = tail.split_whitespace();
    let seconds = match (
        words.next().and_then(|n| n.parse::<f64>().ok()),
        words.next(),
    ) {
        (Some(value), Some(unit)) if unit.starts_with("second") => Some(value),
        _ => None,
    };

    (status, (!url.is_empty()).then_some(url), seconds)
}

fn is_http_status(code: &u16) -> bool {
    (100..600).contains(code)
}

/// A status code written as a number or as a string, as formatters differ.
fn status_code(value: &Value) -> Option<u16> {
    let code = match value {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    u16::try_from(code).ok().filter(is_http_status)
}

fn non_negative_number(value: &Value) -> Option<f64> {
    let raw = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (raw >= 0.0 && raw.is_finite()).then_some(raw)
}

/// `https://user:sk_live_x@api.example.com/v1/customers/4711/orders?key=…`
/// → `api.example.com/v1/customers/#/orders`
///
/// Three things go, in this order and for three different reasons: the query
/// string and the fragment because they carry the credentials, the userinfo
/// because it carries them too, and the scheme because `http` and `https` to
/// one host are one provider. What is left is folded the way an error
/// signature is, so that one customer per row does not become the table.
pub fn http_target(url: &str) -> String {
    let url = url.trim();
    // Cut before anything else: whatever follows must not be read at all, not
    // even to be folded.
    let url = &url[..url.find(['?', '#']).unwrap_or(url.len())];
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);

    let (authority, path) = match after_scheme.find('/') {
        Some(slash) => after_scheme.split_at(slash),
        None => (after_scheme, ""),
    };
    // A host is case-insensitive; the credentials before the `@` are not part
    // of it. The last `@` wins: a password may itself contain one.
    let host = authority.rsplit('@').next().unwrap_or(authority);

    let mut target = String::with_capacity(url.len());
    target.push_str(&host.to_ascii_lowercase());
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        target.push('/');
        push_segment(&mut target, segment);
    }
    // A key coming from the logs is bounded like every other.
    truncate_chars(&mut target, 200);
    target
}

/// Writes one path segment, folded when it identifies one thing rather than
/// naming a kind of thing.
fn push_segment(target: &mut String, segment: &str) {
    // A file keeps its extension: `#.json` and `#.jpg` are two different
    // things being served, and only the name in front of them varies. A CDN
    // path is the shape most likely to reach the ceiling otherwise.
    if let Some((stem, extension)) = split_extension(segment)
        && is_identifier(stem)
    {
        target.push('#');
        target.push('.');
        target.push_str(extension);
    } else if is_identifier(segment) {
        target.push('#');
    } else {
        target.push_str(segment);
    }
}

/// `f47ac10b-….json` → `("f47ac10b-…", "json")`. Only a short alphanumeric
/// suffix counts as an extension: a segment full of dots is not a file name.
fn split_extension(segment: &str) -> Option<(&str, &str)> {
    let (stem, extension) = segment.rsplit_once('.')?;
    (!stem.is_empty()
        && (1..=5).contains(&extension.len())
        && extension.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then_some((stem, extension))
}

/// A path segment that identifies one thing rather than naming a kind of
/// thing: all digits, a UUID, or a long hexadecimal blob.
///
/// A segment mixing letters and digits — `v1`, `oauth2` — is a name and stays:
/// folding it would merge two versions of an API into one row, and the whole
/// point of the shape is to keep what distinguishes a route from a record.
fn is_identifier(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    bytes.iter().all(u8::is_ascii_digit)
        || is_uuid(segment)
        || (bytes.len() >= 16 && bytes.iter().all(u8::is_ascii_hexdigit))
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// The message without its `User Deprecated: ` / `Deprecated: ` prefix, or
/// `None` if it carries neither — in which case it is not a deprecation.
fn deprecation_body(message: &str) -> Option<&str> {
    message
        .strip_prefix("User Deprecated: ")
        .or_else(|| message.strip_prefix("Deprecated: "))
}

/// Extracts `/path/File.php:42` from `[object] (Foo(code: 0): … at /path/File.php:42)`.
///
/// The first line only: with `include_stacktraces`, the trace follows on the
/// next ones, and so does a chained `[previous exception]`.
fn origin_from_object_string(s: &str) -> Option<&str> {
    let head = s.lines().next()?.trim_end();
    let head = head.strip_suffix(')').unwrap_or(head);
    let (_, origin) = head.rsplit_once(" at ")?;
    let origin = origin.trim();
    (!origin.is_empty()).then_some(origin)
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

/// Whether a quote closes the string it sits in, judging by what follows it.
///
/// A string ends at the end of the message or before a separator; a quote
/// followed by anything else opens one **inside** it. That is how Symfony
/// writes an exception: the whole message between quotes, and the part that
/// varies quoted again within — `: "No route found for "GET /x"" at …`.
/// Pairing quotes left to right there closes the outer string on the inner
/// opening one, which folds away the stable sentence and keeps the varying
/// path as the key.
fn ends_a_string(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(c) => {
            c.is_whitespace()
                || matches!(c, ',' | ';' | ':' | '.' | ')' | ']' | '}' | '"' | '!' | '?')
        }
    }
}

/// Writes `src` into `dst`, erasing everything that varies between occurrences.
fn normalize_into(src: &str, dst: &mut String) {
    let mut chars = src.chars().peekable();
    let mut last_was_digit = false;
    // Depth of nested quoted strings: everything from the outermost opening
    // quote to its matching close is one varying value, however many quotes
    // sit inside it.
    let mut depth = 0usize;

    while let Some(c) = chars.next() {
        // A quoted string is almost always a varying value (an id, a file
        // name, a route): replace it wholesale.
        if c == '"' {
            if depth == 0 {
                dst.push_str("\"…\"");
                last_was_digit = false;
            }
            // A message whose quotes never close — `he said "hi`, or a line
            // cut at the ceiling — swallows its tail rather than reopening a
            // string on every quote: the key stays stable either way.
            depth = if depth == 0 {
                1
            } else if ends_a_string(chars.peek().copied()) {
                depth - 1
            } else {
                depth + 1
            };
            continue;
        }
        if depth > 0 {
            continue;
        }
        match c {
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

/// Folds a cache key the way an error signature is folded: a run of digits
/// becomes `#`, so `product_42_teasers` and `product_1337_teasers` are one
/// cache entry family and not one row per product.
pub fn normalize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len().min(200));
    normalize_into(key.trim(), &mut out);
    truncate_chars(&mut out, 200);
    out
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
    fn a_message_quoted_inside_the_exception_quotes_folds_to_one_key() {
        // Symfony quotes the whole message, and the message quotes the part
        // that varies. Pairing quotes left to right closed the outer string on
        // the inner opening one: the sentence was folded away and the path
        // kept, so one defect came out as one row per image.
        let line = |path: &str| {
            format!(
                r#"[2026-09-09T10:23:46+02:00] request.CRITICAL: Uncaught PHP Exception InvalidArgumentException: "The controller for URI "{path}" is not callable: Root image path not resolvable "/var/uploads"" at ControllerResolver.php line 97 {{}} []"#
            )
        };
        let one = parse_line(&line("/media/postcard_320/sample.jpg")).unwrap();
        let other = parse_line(&line("/media/listing_640/cover.jpg")).unwrap();

        assert_eq!(one.signature(), other.signature());
        assert_eq!(
            one.signature(),
            r#"Uncaught PHP Exception InvalidArgumentException: "…" at ControllerResolver.php line #"#
        );

        // The same rule merges what only a scheme separated: the identical
        // missing route used to count twice, once per protocol.
        let route = |scheme: &str| {
            format!(
                r#"[2026-09-09T10:23:46+02:00] request.ERROR: Uncaught PHP Exception NotFoundHttpException: "No route found for "GET {scheme}://localhost/sw.js" (from "{scheme}://localhost/sw.js")" at RouterListener.php line 156 {{}} []"#
            )
        };
        assert_eq!(
            parse_line(&route("http")).unwrap().signature(),
            parse_line(&route("https")).unwrap().signature()
        );
    }

    #[test]
    fn two_values_side_by_side_stay_two_folds() {
        // The counterpart: a message carrying two values one after the other
        // is not nesting, and the sentence between them is what makes the key
        // readable.
        let line = r#"[2026-09-09T10:23:46+02:00] console.DEBUG: Command "app:import" exited with code "1146" {} []"#;
        assert_eq!(
            parse_line(line).unwrap().signature(),
            r#"Command "…" exited with code "…""#
        );
    }

    #[test]
    fn a_deprecation_is_recognised_with_its_origin() {
        // What Symfony's ErrorHandler writes: the level's name in front of
        // the message, an ErrorException in the context pointing at the
        // deprecated code — here the string form of the LineFormatter.
        let line = r#"[2026-09-12T10:23:45+02:00] php.INFO: User Deprecated: Since symfony/http-foundation 6.2: Calling "Symfony\Component\HttpFoundation\Request::getContentType()" is deprecated, use "getContentTypeFormat()" instead. {"exception":"[object] (ErrorException(code: 0): User Deprecated: Since symfony/http-foundation 6.2: Calling \"Symfony\\Component\\HttpFoundation\\Request::getContentType()\" is deprecated, use \"getContentTypeFormat()\" instead. at /var/www/vendor/symfony/http-foundation/Request.php:1290)"} []"#;
        let e = parse_line(line).expect("the line must be recognised");
        assert!(e.is_deprecation());
        assert_eq!(
            e.exception_origin(),
            Some("/var/www/vendor/symfony/http-foundation/Request.php:1290")
        );

        // The prefix goes, the identifiers fold, and so does the line number:
        // a deployment in the middle of the file must not split the row.
        let (message, origin) = e.deprecation_key();
        assert!(message.starts_with("Since symfony/http-foundation #.#: Calling"));
        assert_eq!(
            origin,
            "/var/www/vendor/symfony/http-foundation/Request.php:#"
        );
        let moved = line.replace("Request.php:1290", "Request.php:1302");
        assert_eq!(
            parse_line(&moved).unwrap().deprecation_key(),
            e.deprecation_key()
        );

        // The JsonFormatter's object form carries `file` already joined.
        let json = r#"{"message":"Deprecated: Creation of dynamic property App\\Entity\\Order::$total is deprecated","context":{"exception":{"class":"ErrorException","message":"Deprecated: Creation of dynamic property App\\Entity\\Order::$total is deprecated","code":0,"file":"/var/www/src/Entity/Order.php:41"}},"level":200,"level_name":"INFO","channel":"php","datetime":"2026-09-12T10:23:45+02:00","extra":{}}"#;
        let e = parse_line(json).unwrap();
        assert!(e.is_deprecation(), "the engine's own deprecations too");
        assert_eq!(
            e.exception_origin(),
            Some("/var/www/src/Entity/Order.php:41")
        );
        assert_eq!(e.deprecation_key().1, "/var/www/src/Entity/Order.php:#");

        // Neither prefix: not a deprecation, whatever the channel says.
        let other = r#"[2026-09-12T10:23:45+02:00] php.INFO: Notice: Undefined index: foo {"exception":"[object] (ErrorException(code: 0): Notice: Undefined index: foo at /var/www/src/X.php:3)"} []"#;
        assert!(!parse_line(other).unwrap().is_deprecation());
        // And a message with no origin at all still yields a key.
        let bare = r#"[2026-09-12T10:23:45+02:00] app.INFO: User Deprecated: Passing 42 to foo() is deprecated {} []"#;
        assert_eq!(
            parse_line(bare).unwrap().deprecation_key(),
            (
                "Passing # to foo() is deprecated".to_string(),
                String::new()
            )
        );
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
    fn console_line(level: &str, message: &str, context: &str) -> LogEntry {
        let line =
            format!("[2026-09-09T10:23:45.123456+02:00] console.{level}: {message} {context} []");
        parse_line(&line).expect("a valid console line")
    }

    #[test]
    fn a_command_run_is_counted_on_the_line_that_says_it_ended() {
        // Symfony writes nothing when a command starts and exactly one line
        // when it ends, so that line — and only that line — is the run.
        let exited = console_line(
            "DEBUG",
            r#"Command "app:import --env=prod" exited with code "1""#,
            r#"{"command":"app:import --env=prod","code":1}"#,
        )
        .command()
        .expect("a run");
        // The arguments are not the command: one row per invocation would
        // answer nothing.
        assert_eq!(exited.name, "app:import");
        assert_eq!(exited.code, Some(1));
        assert!(!exited.threw);

        // The exception line names the command too, but it is not a second
        // run: the exit line will come for the same one.
        let threw = console_line(
            "CRITICAL",
            r#"Error thrown while running command "app:import". Message: "Connection refused""#,
            r#"{"command":"app:import","message":"Connection refused"}"#,
        )
        .command()
        .expect("a failure");
        assert_eq!(threw.name, "app:import");
        assert_eq!(threw.code, None, "no code: this line is not the end");
        assert!(threw.threw);
    }

    #[test]
    fn a_console_line_naming_no_command_is_left_alone() {
        // `The console exited with code "1"` is what Symfony writes when the
        // input resolved to no command at all. There is nothing to group it
        // under, and a row named for the console itself answers no question.
        for (level, message, context) in [
            (
                "DEBUG",
                r#"The console exited with code "1""#,
                r#"{"code":1}"#,
            ),
            (
                "CRITICAL",
                r#"An error occurred while using the console. Message: "Boom""#,
                r#"{"message":"Boom"}"#,
            ),
            ("DEBUG", "Some other console line", "{}"),
            // A placeholder left as written is not a command name.
            (
                "DEBUG",
                r#"Command "{command}" exited with code "{code}""#,
                "[]",
            ),
        ] {
            assert!(
                console_line(level, message, context).command().is_none(),
                "{message}"
            );
        }

        // The channel is the gate.
        let elsewhere = r#"[2026-09-09T10:23:45.123456+02:00] app.DEBUG: Command "app:import" exited with code "0" {"command":"app:import","code":0} []"#;
        assert!(parse_line(elsewhere).unwrap().command().is_none());
    }

    #[test]
    fn the_exit_code_is_read_from_the_message_when_the_context_is_gone() {
        // Some formatters drop the context; the code is still the last quoted
        // value of the line, the name being the first.
        let line = console_line(
            "DEBUG",
            r#"Command "app:import" exited with code "137""#,
            "[]",
        )
        .command()
        .expect("a run");
        assert_eq!(line.name, "app:import");
        assert_eq!(line.code, Some(137));
    }

    fn cache_line(message: &str, context: &str) -> LogEntry {
        let line = format!("[2026-09-09T10:23:45.123456+02:00] cache.INFO: {message} {context} []");
        parse_line(&line).expect("a valid cache line")
    }

    #[test]
    fn every_cache_line_symfony_writes_is_a_miss() {
        // Symfony logs when it computes an item and stays silent when it
        // serves one, so there is no hit to read: the question is only who
        // paid for the miss.
        let context = r#"{"key":"homepage_teasers"}"#;
        for (message, event) in [
            (
                r#"Lock acquired, now computing item "homepage_teasers""#,
                CacheEvent::Computed,
            ),
            // `%s` in the template is "acquired" or "not supported"; both mean
            // this process is the one computing.
            (
                r#"Lock not supported, now computing item "homepage_teasers""#,
                CacheEvent::Computed,
            ),
            (
                r#"Lock acquired, now computing item "{key}""#,
                CacheEvent::Computed,
            ),
            (
                r#"Item "homepage_teasers" is locked, waiting for it to be released"#,
                CacheEvent::Contended,
            ),
        ] {
            let line = cache_line(message, context)
                .cache_miss()
                .unwrap_or_else(|| panic!("not read: {message}"));
            assert_eq!(line.event, event, "{message}");
            assert_eq!(line.key, "homepage_teasers", "{message}");
        }

        // What follows a wait is its outcome, not a second miss: counting
        // those would double the contention.
        for message in [
            r#"Item "homepage_teasers" retrieved after lock was released"#,
            r#"Item "homepage_teasers" not found while lock was released, now retrying"#,
        ] {
            assert!(
                cache_line(message, context).cache_miss().is_none(),
                "{message}"
            );
        }

        // The channel is the gate, and a placeholder is not a key.
        let elsewhere = r#"[2026-09-09T10:23:45.123456+02:00] app.INFO: Lock acquired, now computing item "x" {"key":"x"} []"#;
        assert!(parse_line(elsewhere).unwrap().cache_miss().is_none());
        assert!(
            cache_line(r#"Lock acquired, now computing item "{key}""#, "[]")
                .cache_miss()
                .is_none(),
            "no context and an uninterpolated key: nothing to group by"
        );
        // With no context, the key is read back out of the message.
        assert_eq!(
            cache_line(r#"Lock acquired, now computing item "nav_menu""#, "[]")
                .cache_miss()
                .expect("a miss")
                .key,
            "nav_menu"
        );
    }

    #[test]
    fn a_cache_key_folds_its_identifiers_like_a_signature() {
        // One cache entry family, not one row per product — and the ceiling
        // is what would otherwise fill up.
        assert_eq!(normalize_key("product_42_detail"), "product_#_detail");
        assert_eq!(
            normalize_key("product_1337_detail"),
            normalize_key("product_42_detail")
        );
        assert_eq!(normalize_key("nav_menu"), "nav_menu");
        assert_eq!(normalize_key("  nav_menu  "), "nav_menu");
        // A key coming from the logs is bounded like every other.
        assert!(normalize_key(&"a".repeat(400)).chars().count() <= 201);
    }

    fn messenger_line(channel: &str, message: &str, context: &str) -> LogEntry {
        let line =
            format!("[2026-09-09T10:23:45.123456+02:00] {channel}.INFO: {message} {context} []");
        parse_line(&line).expect("a valid messenger line")
    }

    #[test]
    fn symfony_s_own_templates_are_read_with_or_without_their_placeholders() {
        // Monolog leaves `{class}` in place unless `PsrLogMessageProcessor` is
        // configured, so the same event arrives in two shapes. Matching the
        // fixed head of each template is what makes them read the same; the
        // class then comes from the context, which is always there.
        let context = r#"{"class":"App\\Message\\IndexEntityMessage"}"#;
        for (message, event) in [
            (
                r"Sending message App\Message\IndexEntityMessage with async sender using X",
                MessageEvent::Dispatched,
            ),
            (
                "Sending message {class} with {alias} sender using {sender}",
                MessageEvent::Dispatched,
            ),
            (
                r"App\Message\IndexEntityMessage was handled successfully (acknowledging to transport).",
                MessageEvent::Handled,
            ),
            ("Message {class} handled by {handler}", MessageEvent::Ran),
            ("No handler for message {class}", MessageEvent::NoHandler),
            (
                "Rejected message {class} will be sent to the failure transport {transport}.",
                MessageEvent::Failed,
            ),
        ] {
            let line = messenger_line("messenger", message, context)
                .messenger()
                .unwrap_or_else(|| panic!("not read: {message}"));
            assert_eq!(line.event, event, "{message}");
            assert_eq!(line.class, r"App\Message\IndexEntityMessage", "{message}");
            assert!(!line.audited, "{message}");
        }
    }

    #[test]
    fn a_retry_and_a_final_failure_start_alike_and_mean_the_opposite() {
        // Both begin `Error thrown while handling message …`. One says the
        // message is coming back, the other that it is gone for good, and
        // reading the first as the second would report an outage every time a
        // transient error was retried.
        let context = r#"{"class":"App\\Message\\Foo","message_id":"42","retryCount":1}"#;
        let retry = messenger_line(
            "messenger",
            r#"Error thrown while handling message App\Message\Foo. Sending for retry #1 using 1000 ms delay. Error: "Boom""#,
            context,
        )
        .messenger()
        .expect("a retry");
        assert_eq!(retry.event, MessageEvent::Retried);
        // The worker writes `message_id` on the lines it produces, even
        // though the dispatch that opened the message carried none.
        assert_eq!(retry.id.as_deref(), Some("42"));

        let gone = messenger_line(
            "messenger",
            r#"Error thrown while handling message App\Message\Foo. Removing from transport after 3 retries. Error: "Boom""#,
            context,
        )
        .messenger()
        .expect("a failure");
        assert_eq!(gone.event, MessageEvent::Failed);
    }

    #[test]
    fn an_audit_middleware_gives_a_dispatch_the_identifier_symfony_withholds() {
        // `[id] Sent Class` is not core Symfony — it is the widespread
        // middleware pattern — and it is the only thing that names a
        // dispatch, since Symfony writes `message_id` on the worker side
        // alone. Without it there is nothing to pair, and no lag.
        let sent = messenger_line(
            "messenger_audit",
            r"[1a2b3c4d5e6f7] Sent App\Message\IndexEntityMessage",
            r#"{"id":"1a2b3c4d5e6f7","class":"App\\Message\\IndexEntityMessage"}"#,
        )
        .messenger()
        .expect("a dispatch");
        assert_eq!(sent.event, MessageEvent::Dispatched);
        assert_eq!(sent.id.as_deref(), Some("1a2b3c4d5e6f7"));
        assert!(sent.audited, "it is not Symfony writing this");

        // The `Received` counterpart carries no context at all: the class has
        // to come out of the message itself.
        let received = messenger_line(
            "messenger_audit",
            r"[1a2b3c4d5e6f7] Received App\Message\IndexEntityMessage",
            "[]",
        )
        .messenger()
        .expect("a handling");
        assert_eq!(received.event, MessageEvent::Handled);
        assert_eq!(received.class, r"App\Message\IndexEntityMessage");
        assert_eq!(received.id.as_deref(), Some("1a2b3c4d5e6f7"));
    }

    #[test]
    fn a_line_that_is_not_messengers_is_not_a_message() {
        // The channel is the gate, and anything else between brackets belongs
        // to whoever wrote it.
        assert!(
            messenger_line(
                "app",
                r"Sending message App\Message\Foo with async sender using X",
                "{}"
            )
            .messenger()
            .is_none()
        );
        for message in [
            r"[abc] Enqueued App\Message\Foo",
            r"[] Sent App\Message\Foo",
            "Stopping worker.",
            "Received message {class}",
        ] {
            assert!(
                messenger_line("messenger", message, "{}")
                    .messenger()
                    .is_none(),
                "{message}"
            );
        }
        // A placeholder left as written is not a class: one row named
        // `{class}` would be a lie, and the context is the only source left.
        assert!(
            messenger_line(
                "messenger",
                "Sending message {class} with {alias} sender using {s}",
                "[]"
            )
            .messenger()
            .is_none()
        );
    }

    /// A real `http_client` line, credentials and all — the line that prompted
    /// the dimension.
    fn outbound_line(context: &str) -> LogEntry {
        let line = format!(
            r#"[2026-09-09T10:23:45.123456+02:00] http_client.INFO: Response: "200 https://api.example.com/v1/geocode?q=12+rue&key=sk_live_9f3c2a" 0.214782 seconds {context} []"#
        );
        parse_line(&line).expect("a valid http_client line")
    }

    #[test]
    fn no_query_string_ever_reaches_an_outbound_shape() {
        // The rule this whole dimension hangs on. A provider's URL carries an
        // API key in plain sight; a shape that kept the query string would put
        // that key in the table, in the JSON and in an export. It is dropped
        // whole rather than folded, so nothing depends on recognising it.
        let call = outbound_line("{}").http_call().expect("a call");
        assert_eq!(call.target, "api.example.com/v1/geocode");
        assert_eq!(call.shape(), "api.example.com/v1/geocode");

        // Wherever it is written, and whatever carries it.
        for url in [
            "https://api.example.com/v1/geocode?key=sk_live_9f3c2a",
            "https://api.example.com/v1/geocode#key=sk_live_9f3c2a",
            "https://api.example.com/v1/geocode?",
            // The credentials sit before the host as readily as after the
            // path, and a password may itself hold an `@`.
            "https://user:sk_live_9f3c2a@api.example.com/v1/geocode",
            "https://user:p@ss@api.example.com/v1/geocode",
        ] {
            let target = http_target(url);
            assert_eq!(target, "api.example.com/v1/geocode", "{url}");
            assert!(!target.contains("sk_live"), "{url}");
        }
    }

    #[test]
    fn an_identifier_in_a_path_folds_but_a_name_does_not() {
        // What varies from one call to the next is the record, not the route:
        // fold the record and there is one row per provider endpoint, keep it
        // and there is one row per customer.
        for (url, expected) in [
            (
                "https://api.example.com/v1/customers/4711/orders",
                "api.example.com/v1/customers/#/orders",
            ),
            // A version is a name, not an identifier: `v1` and `v2` are two
            // different APIs and must not merge.
            (
                "https://api.example.com/v2/geocode",
                "api.example.com/v2/geocode",
            ),
            (
                "https://api.example.com/oauth2/token",
                "api.example.com/oauth2/token",
            ),
            (
                "https://api.example.com/users/f47ac10b-58cc-4372-a567-0e02b2c3d479",
                "api.example.com/users/#",
            ),
            // A CDN path: the name is an identifier, the extension says what
            // is being served and stays.
            (
                "https://cdn.test/catalogue/f47ac10b-58cc-4372-a567-0e02b2c3d479.json",
                "cdn.test/catalogue/#.json",
            ),
            (
                "https://cdn.test/i/9f3c2a7b1d4e6f80aa/thumb.jpg",
                "cdn.test/i/#/thumb.jpg",
            ),
            // A host is case-insensitive, the scheme says nothing about which
            // provider it is, and a trailing slash is not a segment.
            ("HTTPS://API.Example.COM/v1/", "api.example.com/v1"),
            ("https://api.example.com", "api.example.com"),
            // A port distinguishes two services on one host: it stays.
            ("http://localhost:8080/health", "localhost:8080/health"),
        ] {
            assert_eq!(http_target(url), expected, "{url}");
        }
    }

    #[test]
    fn an_outbound_call_carries_its_status_and_its_duration() {
        // The bare formatter writes them into the message; an application
        // logging the info array puts them in the context, and the context
        // wins — it holds what curl measured, not what was formatted.
        let from_message = outbound_line("{}").http_call().expect("a call");
        assert_eq!(from_message.status, Some(200));
        assert_eq!(from_message.seconds, Some(0.214782));
        assert_eq!(from_message.method, None, "the message names no verb");

        let from_context = outbound_line(
            r#"{"http_method":"get","http_code":429,"total_time":1.5,"url":"https://api.example.com/v1/geocode?key=sk_live_9f3c2a"}"#,
        )
        .http_call()
        .expect("a call");
        assert_eq!(from_context.status, Some(429), "the provider's own answer");
        assert_eq!(from_context.seconds, Some(1.5));
        assert_eq!(from_context.method.as_deref(), Some("GET"));
        assert_eq!(from_context.shape(), "GET api.example.com/v1/geocode");
    }

    #[test]
    fn the_request_announcement_is_not_a_call() {
        // Symfony writes two lines per call. Only the second carries the
        // status and the duration; counting both would double every call.
        let line = r#"[2026-09-09T10:23:45.123456+02:00] http_client.INFO: Request: "GET https://api.example.com/v1/geocode?key=sk_live_9f3c2a" [] []"#;
        assert!(parse_line(line).unwrap().http_call().is_none());

        // And a line from anywhere else is not one either, whatever it holds.
        let elsewhere = r#"[2026-09-09T10:23:45.123456+02:00] request.INFO: Request finished {"url":"https://shop.test/x","status":200} []"#;
        assert!(parse_line(elsewhere).unwrap().http_call().is_none());
    }

    #[test]
    fn an_outbound_call_names_no_endpoint_and_no_response_of_ours() {
        // `url` on an http_client line is the third party's address. Read as a
        // request URI it would become a row of the endpoint table — carrying
        // the API key with it — and its 429 would count among the statuses we
        // served.
        let entry = outbound_line(
            r#"{"url":"https://api.example.com/v1/geocode?key=sk_live_9f3c2a","http_code":429,"status":429}"#,
        );
        assert_eq!(entry.request_uri(), None);
        assert_eq!(entry.endpoint(), None);
        assert_eq!(entry.status(), None);
        assert_eq!(entry.http_call().unwrap().status, Some(429));
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

    const TEMPLATES: [&str; 10] = [
        r#"[2026-09-09T10:23:45.123456+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\Exception\Boom(code: 0): nope)","route":"app_home"} {"token":"aaa"}"#,
        r#"{"message":"Matched route","context":{"route":"app_home","duration_ms":12.5},"level":200,"channel":"request","datetime":"2026-09-09T10:23:45.123456+02:00"}"#,
        r#"[2026-09-09T10:23:45.123456+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT t0.id FROM produit t0 WHERE t0.id = ?","params":{"1":42}} []"#,
        r#"[2026-09-09T10:23:45.123456+02:00] http_client.INFO: Response: "200 https://api.example.com/v1/geocode?q=x&key=sk_live_9f3c" 0.214782 seconds {"http_code":200,"total_time":0.214782,"url":"https://api.example.com/v1/geocode?q=x&key=sk_live_9f3c"} []"#,
        r#"[2026-09-09T10:23:45.123456+02:00] messenger_audit.INFO: [1a2b3c4d5e6f7] Sent App\Message\IndexEntityMessage {"id":"1a2b3c4d5e6f7","class":"App\\Message\\IndexEntityMessage"} []"#,
        r#"[2026-09-09T10:23:45.123456+02:00] cache.INFO: Lock acquired, now computing item "homepage_teasers" {"key":"homepage_teasers"} []"#,
        r#"[2026-09-09T10:23:45.123456+02:00] console.DEBUG: Command "app:import --env=prod" exited with code "1" {"command":"app:import --env=prod","code":1} []"#,
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
            // Quotes that never balance: a message cut mid-string, an odd
            // count, nothing but quotes.
            "[2026-09-09T10:23:45+02:00] a.ERROR: he said \"hi {} []",
            "[2026-09-09T10:23:45+02:00] a.ERROR: \"a\"b\"c\" x {} []",
            "[2026-09-09T10:23:45+02:00] a.ERROR: \"\"\"\" {} []",
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
        let _ = entry.http_call();
        let _ = entry.messenger();
        let _ = entry.cache_miss();
        let _ = entry.command();
        let mut copy = entry.message.clone();
        truncate_chars(&mut copy, 7);
        assert!(copy.chars().count() <= 8, "truncation stays bounded");
    }

    #[test]
    fn a_malformed_outbound_line_yields_nothing_rather_than_nonsense() {
        for message in [
            "Response:",
            r#"Response: """#,
            r#"Response: "200 ""#,
            "Response: no quotes at all",
            r#"Response: "200 https://api.example.com/x" 42"#,
        ] {
            let line =
                format!("[2026-09-09T10:23:45.123456+02:00] http_client.INFO: {message} [] []");
            let entry = parse_line(&line).expect("a valid line");
            // Whatever comes out, it must never be a duration invented from a
            // bare number sitting after the quote.
            if let Some(call) = entry.http_call() {
                assert!(!call.target.is_empty(), "{message}");
                assert_eq!(call.seconds, None, "{message}");
            }
        }
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
