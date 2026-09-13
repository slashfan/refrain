//! Realistic Symfony/Monolog log generator, to test `refrain` without
//! production data.
//!
//! It replays the typical sequence of a Symfony request: `Matched route`, a
//! little `security` and `doctrine`, sometimes an exception, then the closing
//! line. Every request carries a `token` in `extra`, exactly as Monolog's
//! `UidProcessor` does — enough to exercise both of refrain's measurement
//! modes.
//!
//! ```bash
//! cargo run --bin genlogs -- --rate 200 var/log/prod.log
//! ```

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeDelta};
use clap::Parser;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "genlogs",
    about = "Generate realistic fake Symfony/Monolog logs"
)]
struct Args {
    /// Output file. Omitted, writes to standard output.
    file: Option<PathBuf>,

    /// Simulated requests per second (0 = as fast as possible).
    #[arg(short, long, default_value_t = 20.0)]
    rate: f64,

    /// Total number of requests (0 = endless).
    #[arg(short, long, default_value_t = 0)]
    count: u64,

    /// Share of failing requests, between 0 and 1.
    #[arg(long, default_value_t = 0.06)]
    error_rate: f64,

    /// JSON format (JsonFormatter) instead of the line format.
    #[arg(long)]
    json: bool,

    /// Skip the final line carrying `duration_ms`: forces refrain to derive
    /// durations by correlating tokens.
    #[arg(long)]
    no_durations: bool,

    /// Write no token in `extra` (disables correlation).
    #[arg(long)]
    no_tokens: bool,

    /// Inject no N+1: every SQL query stays distinct.
    #[arg(long)]
    no_nplus1: bool,

    /// Log no outbound HTTP call on the `http_client` channel.
    #[arg(long)]
    no_http_client: bool,

    /// Log no Messenger line on the `messenger` channel.
    #[arg(long)]
    no_messenger: bool,

    /// Share of dispatched messages a worker gets to handle, between 0 and 1.
    /// Below 1 the queue does not drain — the consumer that died on Friday.
    #[arg(long, default_value_t = 0.35, value_name = "SHARE")]
    handled_share: f64,

    /// Spread the requests over the last N seconds instead of stamping them
    /// all with now. Requires `--count`.
    ///
    /// Without it, `--rate 0` writes everything within a handful of
    /// milliseconds: refrain's graphs shrink to a single bar and `--since` has
    /// nothing to cut. The simulated rate is not flat either — it rises then
    /// falls, like real traffic.
    #[arg(long, default_value_t = 0.0, value_name = "SEC")]
    spread: f64,

    /// Seed of the random generator, to replay the same sequence.
    #[arg(long, default_value_t = 0x5eed_1234_9abc_def0)]
    seed: u64,
}

/// (route name, URL template, median latency in ms)
const ROUTES: [(&str, &str, f64); 8] = [
    ("app_home", "/", 30.0),
    ("app_product_show", "/product/{id}", 75.0),
    ("app_product_list", "/products", 130.0),
    ("app_cart_add", "/cart/add", 55.0),
    ("app_checkout", "/checkout", 380.0),
    ("app_search", "/search", 240.0),
    ("api_orders_list", "/api/orders", 850.0),
    ("app_login", "/login", 50.0),
];

/// Prepared statements, as Doctrine logs them: the parameters are kept apart,
/// so two executions of one pattern have exactly the same text.
const QUERIES: [&str; 6] = [
    "SELECT t0.id, t0.name, t0.price FROM product t0 WHERE t0.id = ?",
    "SELECT t0.id, t0.label FROM category t0 WHERE t0.id = ?",
    "SELECT t0.id, t0.total, t0.created_at FROM orders t0 WHERE t0.customer_id = ?",
    "SELECT t0.id, t0.street, t0.city FROM address t0 WHERE t0.customer_id = ?",
    "SELECT t0.id, t0.qty, t0.product_id FROM order_item t0 WHERE t0.order_id = ?",
    "SELECT t0.id, t0.email FROM customer t0 WHERE t0.id = ?",
];

/// Outbound calls, as Symfony's HttpClient logs them. The query strings carry
/// an API key and a customer's address, exactly as the real ones do: that is
/// what the "no query string ever reaches an output" rule is exercised on.
///
/// (method, URL template, median latency in ms)
const OUTBOUND: [(&str, &str, f64); 4] = [
    (
        "GET",
        "https://api.geocoder.test/v1/geocode?q=12+rue+des+Lilas&key=sk_live_9f3c2a7b",
        180.0,
    ),
    (
        "POST",
        "https://api.payments.test/v2/charges?api_key=pk_live_4d2e8c1f",
        520.0,
    ),
    (
        "GET",
        "https://api.inventory.test/v1/products/{id}/stock",
        70.0,
    ),
    (
        "GET",
        "https://cdn.assets.test/catalogue/f47ac10b-58cc-4372-a567-0e02b2c3d479.json",
        45.0,
    ),
];

/// Messages on the bus, with the chance a request dispatches one.
///
/// (class, probability per request)
const MESSAGES: [(&str, f64); 3] = [
    (r"App\Message\IndexEntityMessage", 0.35),
    (r"App\Message\SendInvoiceMessage", 0.12),
    (r"App\Message\ThumbnailMessage", 0.20),
];

/// The route that dispatches one message per row — the N+1 on the bus, one
/// layer above the SQL one and costlier, since each message is a job.
const MESSAGE_LOOP: (&str, usize, f64) = ("app_product_list", 0, 0.45);

/// Routes that call a third party in a loop — the N+1 no index will fix. The
/// geocoder, once per line of an order.
const OUTBOUND_LOOP: [(&str, usize, f64); 2] =
    [("api_orders_list", 0, 0.6), ("app_checkout", 1, 0.4)];

/// Routes afflicted with an N+1, with its probability of appearing — the
/// classic loop reloading a related entity on every iteration.
const NPLUS1_ROUTES: [(&str, f64); 3] = [
    ("app_product_list", 0.55),
    ("api_orders_list", 0.75),
    ("app_checkout", 0.30),
];

const EXCEPTIONS: [(&str, &str, &str); 5] = [
    (
        r"App\Exception\ProductNotFound",
        "Product {id} not found",
        "src/Controller/ProductController.php",
    ),
    (
        r"Doctrine\DBAL\Exception\ConnectionLost",
        "SQLSTATE[HY000]: server has gone away",
        "vendor/doctrine/dbal/src/Connection.php",
    ),
    (
        r"Symfony\Component\HttpKernel\Exception\NotFoundHttpException",
        "No route found for \"GET /old/{id}\"",
        "vendor/symfony/http-kernel/EventListener/RouterListener.php",
    ),
    (
        r"App\Exception\PaymentDeclined",
        "Payment declined for order {id}",
        "src/Service/Checkout.php",
    ),
    (
        r"Symfony\Component\Cache\Exception\CacheException",
        "Redis connection timed out after {id} ms",
        "vendor/symfony/cache/Adapter/RedisAdapter.php",
    ),
];

/// Deprecations, as Symfony's `ErrorHandler` logs them on the `php` channel
/// at INFO: the level's name in front of the message, an `ErrorException` in
/// the context pointing at the deprecated code — the vendor file for a
/// `trigger_deprecation()`, the compiled template for Twig's `deprecated`
/// tag. Neither names the route: one deprecation reached from every route
/// is one row in refrain, and that is what these exercise.
///
/// (message, origin, probability per request)
const DEPRECATIONS: [(&str, &str, f64); 2] = [
    (
        r#"Since symfony/http-foundation 6.2: Calling "Symfony\Component\HttpFoundation\Request::getContentType()" is deprecated, use "getContentTypeFormat()" instead."#,
        "vendor/symfony/http-foundation/Request.php:1290",
        0.08,
    ),
    (
        r#"The "legacy/layout.html.twig" template is deprecated, extend "base.html.twig" instead."#,
        "var/cache/prod/twig/3f/3f8c0e2b7a1d9c4e5f6a7b8c9d0e1f2a.php:58",
        0.04,
    ),
];

/// xorshift64* generator: a few lines, no dependency, and plenty "random"
/// enough to fabricate logs.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A float in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn hex(&mut self, len: usize) -> String {
        let mut out = String::with_capacity(len);
        while out.len() < len {
            out.push_str(&format!("{:016x}", self.next_u64()));
        }
        out.truncate(len);
        out
    }

    /// Latency: median × a long-tailed factor, plus a few spikes.
    fn latency(&mut self, median: f64) -> f64 {
        let u = self.unit();
        let factor = 0.55 + u * u * u * 4.0;
        let spike = if self.unit() < 0.01 { 8.0 } else { 1.0 };
        median * factor * spike
    }
}

struct Writer {
    out: Box<dyn Write>,
    json: bool,
}

impl Writer {
    /// A Monolog entry, in one format or the other.
    fn entry(
        &mut self,
        at: DateTime<Local>,
        channel: &str,
        level: (&str, i32),
        message: &str,
        context: &str,
        token: Option<&str>,
    ) -> std::io::Result<()> {
        let extra = match token {
            Some(token) => format!(r#"{{"token":"{token}"}}"#),
            None => "[]".to_string(),
        };

        if self.json {
            let extra = if token.is_some() {
                extra.as_str()
            } else {
                "{}"
            };
            writeln!(
                self.out,
                r#"{{"message":{},"context":{context},"level":{},"level_name":"{}","channel":"{channel}","datetime":"{}","extra":{extra}}}"#,
                json_string(message),
                level.1,
                level.0,
                at.format("%Y-%m-%dT%H:%M:%S%.6f%:z"),
            )
        } else {
            writeln!(
                self.out,
                "[{}] {channel}.{}: {message} {context} {extra}",
                at.format("%Y-%m-%dT%H:%M:%S%.6f%:z"),
                level.0,
            )
        }
    }
}

/// Escapes a string destined for the inside of a JSON string. Without this, a
/// message containing quotes ("No route found for \"GET /x\"") would produce an
/// invalid context — exactly the kind of broken log Monolog does not write.
fn json_inner(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Minimal escaping to put a string inside JSON.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut rng = Rng(args.seed | 1);

    let out: Box<dyn Write> = match &args.file {
        Some(path) => {
            // `create(true)` creates the file, not the directories carrying
            // it: `var/log/` does not exist in a freshly cloned repository.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            Box::new(BufWriter::new(file))
        }
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    let mut writer = Writer {
        out,
        json: args.json,
    };

    let pause = if args.rate > 0.0 {
        Duration::from_secs_f64(1.0 / args.rate)
    } else {
        Duration::ZERO
    };

    let mut done = 0u64;
    let maintenant = Local::now();
    while args.count == 0 || done < args.count {
        let start = repartir(&args, done, maintenant);
        emit_request(&mut writer, &mut rng, &args, start)?;
        writer.out.flush()?;
        done += 1;
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
    Ok(())
}

/// At what instant to place request number `done`.
///
/// Without `--spread`, that is now. With it, the requests spread over the
/// window asked for, oldest to most recent — but not at a constant interval:
/// the position is slightly distorted by a sine, which makes the density vary
/// fourfold and gives graphs that look like traffic rather than a wall.
fn repartir(args: &Args, done: u64, maintenant: DateTime<Local>) -> DateTime<Local> {
    if args.spread <= 0.0 || args.count == 0 {
        return Local::now();
    }
    let fraction = done as f64 / args.count as f64;
    // The amplitude stays below 1: beyond that the distortion would stop
    // being monotonic and the requests would come out in the wrong order.
    const AMPLITUDE: f64 = 0.6;
    let deforme =
        fraction + AMPLITUDE * (std::f64::consts::TAU * fraction).sin() / std::f64::consts::TAU;
    let recul = args.spread * (1.0 - deforme);
    maintenant - TimeDelta::milliseconds((recul * 1000.0) as i64)
}

fn emit_request(
    writer: &mut Writer,
    rng: &mut Rng,
    args: &Args,
    start: DateTime<Local>,
) -> std::io::Result<()> {
    let (route, template, median) = ROUTES[rng.below(ROUTES.len())];
    let id = 1 + rng.below(9999);
    let uri = template.replace("{id}", &id.to_string());
    let duration = rng.latency(median);
    let failed = rng.unit() < args.error_rate;

    let token_owned = rng.hex(6);
    let token = (!args.no_tokens).then_some(token_owned.as_str());

    // The lines are spread over the simulated duration: that is what makes
    // measurement by correlation realistic.
    let at = |fraction: f64| start + TimeDelta::microseconds((duration * fraction * 1000.0) as i64);

    writer.entry(
        start,
        "request",
        ("INFO", 200),
        &format!(r#"Matched route "{route}"."#),
        &format!(
            r#"{{"route":"{route}","route_parameters":{{"_route":"{route}","_controller":"App\\Controller\\{}Controller::index","id":"{id}"}},"request_uri":"https://shop.test{uri}","method":"GET"}}"#,
            camel(route)
        ),
        token,
    )?;

    writer.entry(
        at(0.08),
        "security",
        ("DEBUG", 100),
        "Checking for authenticator support.",
        r#"{"firewall_name":"main","authenticators":2}"#,
        token,
    )?;

    // A few distinct queries, as on a normal page.
    for i in 0..(1 + rng.below(3)) {
        let sql = QUERIES[rng.below(QUERIES.len())];
        emit_query(writer, at(0.10 + i as f64 * 0.06), sql, id, token)?;
    }

    // Then, on some routes, the loop that reloads the same entity.
    let afflige = NPLUS1_ROUTES.iter().find(|(name, _)| *name == route);
    if let Some((_, chance)) = afflige
        && !args.no_nplus1
        && rng.unit() < *chance
    {
        let repetitions = 12 + rng.below(45);
        let sql = QUERIES[rng.below(QUERIES.len())];
        for k in 0..repetitions {
            let quand = at(0.25 + 0.35 * k as f64 / repetitions as f64);
            emit_query(writer, quand, sql, id + k, token)?;
        }
    }

    // Outbound calls: a couple on a normal page, and on some routes the same
    // provider called once per row.
    if !args.no_http_client {
        for i in 0..rng.below(3) {
            let call = OUTBOUND[rng.below(OUTBOUND.len())];
            emit_outbound(writer, rng, at(0.30 + i as f64 * 0.05), call, id, token)?;
        }
        if let Some((_, index, chance)) = OUTBOUND_LOOP.iter().find(|(name, _, _)| *name == route)
            && rng.unit() < *chance
        {
            let call = OUTBOUND[*index];
            let repetitions = 4 + rng.below(8);
            for k in 0..repetitions {
                let quand = at(0.45 + 0.20 * k as f64 / repetitions as f64);
                emit_outbound(writer, rng, quand, call, id + k, token)?;
            }
        }
    }

    // Messages handed to the bus. Both vocabularies at once, as a real
    // application running an audit middleware alongside Symfony's own logging
    // writes them — refrain must count one dispatch, not two.
    if !args.no_messenger {
        let mut dispatched = Vec::new();
        for (class, chance) in MESSAGES {
            if rng.unit() < chance {
                dispatched.push(class);
            }
        }
        let (loop_route, loop_index, loop_chance) = MESSAGE_LOOP;
        if route == loop_route && rng.unit() < loop_chance {
            // One message per row of the listing: the N+1 on the bus.
            for _ in 0..(6 + rng.below(20)) {
                dispatched.push(MESSAGES[loop_index].0);
            }
        }
        for (i, class) in dispatched.iter().enumerate() {
            let when = at(0.55 + 0.15 * i as f64 / dispatched.len().max(1) as f64);
            let id = rng.hex(13);
            emit_dispatch(writer, when, class, &id, token)?;
            // The worker takes only its share: below 1, the queue grows, and
            // that is the consumer nobody noticed had died.
            if rng.unit() < args.handled_share {
                emit_handled(writer, rng, when, class, &id)?;
            }
        }
    }

    if failed {
        let (class, template, file) = EXCEPTIONS[rng.below(EXCEPTIONS.len())];
        let message = template.replace("{id}", &id.to_string());
        let line = 40 + rng.below(200);
        writer.entry(
            at(0.7),
            "request",
            ("CRITICAL", 500),
            &format!(
                r#"Uncaught PHP Exception {class}: "{message}" at /var/www/{file} line {line}"#
            ),
            &format!(
                r#"{{"exception":"[object] ({}(code: 0): {} at /var/www/{file}:{line})"}}"#,
                json_inner(class),
                json_inner(&message)
            ),
            token,
        )?;
        // A multi-line stack trace, as in dev: refrain must glue it back onto
        // the previous entry instead of counting it as noise.
        if !args.json && rng.unit() < 0.5 {
            writeln!(
                writer.out,
                "  #0 /var/www/{file}({line}): App\\Service\\Loader->load()"
            )?;
            writeln!(
                writer.out,
                "  #1 /var/www/src/Controller/{}Controller.php(52): App\\Service\\Loader->fetch()",
                camel(route)
            )?;
            writeln!(writer.out, "  #2 {{main}}")?;
        }
    }

    // Deprecations are independent of failure: the deprecated call runs on
    // the healthy requests too, and that is where they pile up.
    for (message, origin, chance) in DEPRECATIONS {
        if rng.unit() < chance {
            let message = format!("User Deprecated: {message}");
            writer.entry(
                at(0.6),
                "php",
                ("INFO", 200),
                &message,
                &format!(
                    r#"{{"exception":"[object] (ErrorException(code: 0): {} at /var/www/{origin})"}}"#,
                    json_inner(&message)
                ),
                token,
            )?;
        }
    }

    if !args.no_durations {
        let status = if failed { 500 } else { 200 };
        writer.entry(
            at(1.0),
            "request",
            ("INFO", 200),
            "Request finished",
            &format!(
                r#"{{"route":"{route}","request_uri":"https://shop.test{uri}","method":"GET","status":{status},"duration_ms":{duration:.1}}}"#
            ),
            token,
        )?;
    }
    Ok(())
}

/// A message handed to a transport. Two lines, as an application running an
/// audit middleware beside Symfony's own logging really writes: the audit one
/// carries the identifier, Symfony's carries none.
fn emit_dispatch(
    writer: &mut Writer,
    at: DateTime<Local>,
    class: &str,
    id: &str,
    token: Option<&str>,
) -> std::io::Result<()> {
    writer.entry(
        at,
        "messenger_audit",
        ("INFO", 200),
        &format!("[{id}] Sent {class}"),
        &format!(r#"{{"id":"{id}","class":"{}"}}"#, json_inner(class)),
        token,
    )?;
    writer.entry(
        at,
        "messenger",
        ("INFO", 200),
        &format!(
            "Sending message {class} with async sender using Messenger\\Transport\\AmqpSender"
        ),
        &format!(
            r#"{{"class":"{}","alias":"async","sender":"Messenger\\Transport\\AmqpSender"}}"#,
            json_inner(class)
        ),
        token,
    )
}

/// A worker taking the message off the queue, some time later — which is what
/// makes the lag worth measuring. It carries no request token: a worker runs
/// outside any HTTP request.
fn emit_handled(
    writer: &mut Writer,
    rng: &mut Rng,
    dispatched_at: DateTime<Local>,
    class: &str,
    id: &str,
) -> std::io::Result<()> {
    let lag = TimeDelta::milliseconds((200.0 + rng.unit() * 9_000.0) as i64);
    let at = dispatched_at + lag;
    // One message in twenty throws and goes back for another attempt.
    if rng.unit() < 0.05 {
        return writer.entry(
            at,
            "messenger",
            ("WARNING", 300),
            &format!(
                "Error thrown while handling message {class}. Sending for retry #1 using 1000 ms delay. Error: \"Connection timed out\""
            ),
            &format!(
                r#"{{"class":"{}","message_id":"{id}","retryCount":1,"delay":1000,"error":"Connection timed out"}}"#,
                json_inner(class)
            ),
            None,
        );
    }
    writer.entry(
        at,
        "messenger_audit",
        ("INFO", 200),
        &format!("[{id}] Received {class}"),
        "[]",
        None,
    )?;
    writer.entry(
        at,
        "messenger",
        ("INFO", 200),
        &format!(
            "Message {class} handled by App\\MessageHandler\\{}Handler",
            short_class(class)
        ),
        &format!(
            r#"{{"class":"{}","handler":"App\\MessageHandler\\{}Handler"}}"#,
            json_inner(class),
            short_class(class)
        ),
        None,
    )?;
    writer.entry(
        at,
        "messenger",
        ("INFO", 200),
        &format!("{class} was handled successfully (acknowledging to transport)."),
        &format!(r#"{{"class":"{}","message_id":"{id}"}}"#, json_inner(class)),
        None,
    )
}

/// `App\Message\ThumbnailMessage` → `Thumbnail`
fn short_class(class: &str) -> &str {
    class
        .rsplit('\\')
        .next()
        .unwrap_or(class)
        .trim_end_matches("Message")
}

/// One outbound call, as Symfony's HttpClient writes it: the announcement,
/// then the response carrying the status and `total_time` — the info array
/// `ResponseInterface::getInfo()` hands back, which is where the verb and the
/// duration live.
fn emit_outbound(
    writer: &mut Writer,
    rng: &mut Rng,
    at: DateTime<Local>,
    (method, template, median): (&str, &str, f64),
    id: usize,
    token: Option<&str>,
) -> std::io::Result<()> {
    let url = template.replace("{id}", &id.to_string());
    let seconds = rng.latency(median) / 1000.0;
    // A third party fails on its own account, and more often than we do.
    let code = match rng.unit() {
        u if u < 0.03 => 429,
        u if u < 0.05 => 503,
        _ => 200,
    };

    writer.entry(
        at,
        "http_client",
        ("INFO", 200),
        &format!(r#"Request: "{method} {url}""#),
        "[]",
        token,
    )?;
    writer.entry(
        at,
        "http_client",
        ("INFO", 200),
        &format!(r#"Response: "{code} {url}" {seconds:.6} seconds"#),
        &format!(
            r#"{{"http_method":"{method}","url":"{}","http_code":{code},"total_time":{seconds:.6}}}"#,
            json_inner(&url)
        ),
        token,
    )
}

fn emit_query(
    writer: &mut Writer,
    at: DateTime<Local>,
    sql: &str,
    id: usize,
    token: Option<&str>,
) -> std::io::Result<()> {
    writer.entry(
        at,
        "doctrine",
        ("DEBUG", 100),
        "Executing statement",
        &format!(r#"{{"sql":"{sql}","params":{{"1":{id}}},"types":{{"1":1}}}}"#),
        token,
    )
}

/// `app_product_show` → `AppProductShow`
fn camel(route: &str) -> String {
    route
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}
