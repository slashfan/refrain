//! End-to-end tests: the real binaries are run, the way a user runs them.
//!
//! The unit tests check each brick in isolation; these check the assembly —
//! that `genlogs` writes a file `refrain` can read back, that a pipe between
//! the two works just as well, that the JSON output really is JSON, and that
//! the exit codes are the ones advertised.
//!
//! `env!("CARGO_BIN_EXE_<name>")` is provided by Cargo: it is the path of the
//! binary it has just built for this test.

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// A clean working directory, distinct per test.
fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("refrain-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn genlogs(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_genlogs"))
        .args(args)
        .output()
        .expect("genlogs must be able to start")
}

fn refrain(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(args)
        .output()
        .expect("refrain must be able to start")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn from_generation_to_json_output() {
    let dir = workdir("json");
    // Deliberately nested: `genlogs` must create the tree, as it does for
    // Symfony's canonical `var/log/` on a brand-new repository.
    let log = dir.join("var").join("log").join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "300", "--seed", "1", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    assert!(log.exists(), "the log file must have been created");

    let out = refrain(&["--json", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));

    let report: Value = serde_json::from_slice(&out.stdout).expect("the output must be JSON");

    let entries = report["totals"]["entries"].as_u64().unwrap();
    assert!(entries > 3000, "too few entries parsed: {entries}");
    assert_eq!(
        report["endpoints"].as_array().unwrap().len(),
        8,
        "the generator's eight routes must come out"
    );
    assert_eq!(report["duration_source"]["kind"], "field");
    assert!(
        !report["nplus1"].as_array().unwrap().is_empty(),
        "the N+1 patterns injected by the generator must be detected"
    );
    // The generator's two deprecations are reached from every route: two
    // rows, however many requests triggered them.
    assert_eq!(
        report["deprecations"].as_array().unwrap().len(),
        2,
        "one row per deprecation, not per route: {}",
        report["deprecations"]
    );
    assert!(report["totals"]["deprecations"].as_u64().unwrap() > 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deprecation_threshold_fails_the_build() {
    let dir = workdir("deprecations");
    let log = dir.join("test.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "200", "--seed", "5", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    // The build fails on the first deprecation, and the summary above the
    // message lists them.
    let out = refrain(&["--summary", "--fail-if", "deprecations>0", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("threshold crossed — deprecations = "),
        "{}",
        stderr(&out)
    );
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Deprecations ("), "{summary}");
    assert!(summary.contains("http-foundation"), "{summary}");

    // A ceiling high enough passes.
    let out = refrain(&["--summary", "--fail-if", "deprecations>100000", path]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_text_summary_reports_the_nplus1_patterns() {
    let dir = workdir("summary");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "200", "--seed", "3", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let out = refrain(&["--summary", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));

    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Slowest endpoints"));
    assert!(summary.contains("N+1 patterns"));
    assert!(summary.contains("api_orders_list"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failing_command_is_told_apart_from_a_failing_request() {
    // The point of the whole dimension: `requests`, `request-error-rate` and
    // the peak are defined over HTTP requests, and a cron job is not one. A
    // nightly import that fails must be findable without moving any of them.
    let dir = workdir("console");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "400", "--seed", "23", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let out = refrain(&["--summary", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Console commands"), "{summary}");
    assert!(summary.contains("app:import"), "{summary}");
    assert!(
        !summary.contains("app:import --env=prod"),
        "the arguments are not the command: {summary}"
    );

    let out = refrain(&["--json", "--top", "0", path]);
    let doc: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("well-formed JSON");
    assert!(doc["console"]["runs"].as_u64().unwrap() > 0);

    // A command is in none of the figures defined over requests.
    let endpoints = doc["endpoints"].as_array().expect("a list");
    assert!(
        !endpoints
            .iter()
            .any(|e| e["endpoint"].as_str().unwrap_or_default().contains(':')),
        "a command must not be an endpoint: {endpoints:?}"
    );
    let commands = doc["commands"].as_array().expect("a list");
    let import = commands
        .iter()
        .find(|c| c["command"] == "app:import")
        .expect("the nightly import");
    assert!(import["runs"].as_u64().unwrap() > 0);
    // It logs a line of its own before it ends, so it can be timed.
    assert!(import["p95_ms"].as_f64().unwrap() > 0.0, "{import}");

    // A log with no console line in it gets no section rather than a zero.
    let quiet = dir.join("quiet.log");
    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "20",
        "--seed",
        "23",
        "--no-console",
        quiet.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    let out = refrain(&["--summary", quiet.to_str().unwrap()]);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Console commands"),
        "no section for a dimension the log does not carry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cache_that_never_hits_shows_in_the_report() {
    // Symfony writes a line when it computes an item and nothing when it
    // serves one, so a key that turns up on nearly every request is a cache
    // doing no work at all — a five-minute fix, invisible until counted.
    let dir = workdir("cache");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "200", "--seed", "17", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let out = refrain(&["--summary", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Cache misses"), "{summary}");
    assert!(summary.contains("nav_menu"), "{summary}");
    // The key that varies per product folds to one row rather than two
    // hundred, which is also what keeps the table bounded.
    assert!(summary.contains("product_#_detail"), "{summary}");

    let out = refrain(&["--json", "--top", "0", path]);
    let doc: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("well-formed JSON");
    let keys = doc["cache_keys"].as_array().expect("a list");
    assert!(keys.len() >= 3, "{keys:?}");
    let worst = &keys[0];
    assert_eq!(worst["key"], "nav_menu");
    let affected = worst["requests_affected"].as_u64().unwrap();
    let requests = doc["totals"]["requests"].as_u64().unwrap();
    assert!(
        affected * 2 > requests,
        "{affected} of {requests} requests recomputed it"
    );
    assert_eq!(doc["cache"]["keys"], keys.len());

    // A log with no cache in it gets no section rather than a row of zeroes.
    let quiet = dir.join("quiet.log");
    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "20",
        "--seed",
        "17",
        "--no-cache",
        quiet.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    let out = refrain(&["--summary", quiet.to_str().unwrap()]);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Cache misses"),
        "no section for a dimension the log does not carry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_queue_that_is_not_draining_shows_and_fails_the_build() {
    // The whole chain on the real binaries. The generator writes both
    // vocabularies for every dispatch — the audit middleware's and Symfony's
    // own — exactly as the log that prompted the dimension does, and lets the
    // worker handle only a share of them.
    let dir = workdir("messenger");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "200",
        "--seed",
        "13",
        "--handled-share",
        "0.25",
        path,
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    // Two lines per dispatch in the file, one dispatch in the report.
    let written = std::fs::read_to_string(&log).unwrap();
    let audit = written.matches("] Sent ").count();
    let core = written.matches("Sending message ").count();
    assert!(audit > 0 && audit == core, "audit {audit}, core {core}");

    let out = refrain(&["--json", "--top", "0", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let doc: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("well-formed JSON");
    let dispatched = doc["messenger"]["dispatched"].as_u64().unwrap();
    assert_eq!(
        dispatched, audit as u64,
        "one dispatch per message, not one per line"
    );
    let handled = doc["messenger"]["handled"].as_u64().unwrap();
    assert!(handled < dispatched, "the worker takes only its share");
    assert_eq!(doc["messenger"]["waiting"], dispatched - handled);

    let out = refrain(&["--summary", path]);
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Messages on the bus"), "{summary}");
    assert!(
        summary.contains("dispatched with no handled line over this read"),
        "{summary}"
    );

    // The threshold a cron job holds: a backlog fails the build.
    let out = refrain(&["--summary", "--fail-if", "messages-waiting>10", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("messages-waiting = "),
        "{}",
        stderr(&out)
    );

    // A log with no bus in it says nothing rather than passing the build on a
    // reassuring zero.
    let quiet = dir.join("quiet.log");
    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "20",
        "--seed",
        "13",
        "--no-messenger",
        quiet.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "messages-waiting>0",
        quiet.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Messages on the bus"),
        "no section for a dimension the log does not carry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_outbound_call_is_reported_without_its_query_string() {
    // The whole chain, on the real binaries: the generator writes the URLs a
    // Symfony application really logs — API key in the query string — and
    // neither the summary, nor the JSON, nor a threshold message may carry
    // that key out of the file it sits in.
    let dir = workdir("outbound");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "200", "--seed", "7", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    let written = std::fs::read_to_string(&log).unwrap();
    assert!(
        written.contains("key=sk_live_9f3c2a7b"),
        "the corpus must hold the key the report must not"
    );

    let out = refrain(&["--summary", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("Outbound HTTP calls"), "{summary}");
    assert!(
        summary.contains("GET api.geocoder.test/v1/geocode"),
        "{summary}"
    );
    assert!(!summary.contains("sk_live"), "{summary}");

    let out = refrain(&["--json", "--top", "0", path]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(!json.contains("sk_live"), "{json}");
    assert!(!json.contains("pk_live"), "{json}");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("well-formed JSON");
    assert!(doc["http_client"]["calls"].as_u64().unwrap() > 0);
    assert!(doc["http_calls"].as_array().unwrap().len() >= 3);

    // And the threshold: a provider going over a second fails the build, and
    // the message names the provider without naming the key.
    let out = refrain(&["--summary", "--fail-if", "http-client-p95>10s", path]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = refrain(&["--summary", "--fail-if", "http-client-p95>1ms", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let message = stderr(&out);
    assert!(
        message.contains("http-client-p95 (POST api.payments.test"),
        "{message}"
    );
    assert!(!message.contains("pk_live"), "{message}");

    // With no outbound call read at all, the threshold says nothing rather
    // than passing the build on a reassuring zero.
    let quiet = dir.join("quiet.log");
    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "20",
        "--seed",
        "7",
        "--no-http-client",
        quiet.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "http-client-p95>1ms",
        quiet.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Outbound HTTP calls"),
        "no section for a dimension the log does not carry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pipe_closed_downstream_is_not_a_failure() {
    // `refrain --json --every 1 … | head -1`, or a collector restarting: the
    // reader goes away, the next write returns EPIPE. That is the normal end of
    // a pipe, not a failure — and above all not exit code 1, which announces "a
    // source could not be read" and would make a cron job believe the logs are
    // unreadable when they have just been read.
    let dir = workdir("pipe");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "50", "--seed", "7", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let mut child = Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(["--json", "--every", "0.2", "--from-start", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("refrain must be able to start");

    let mut reader = BufReader::new(child.stdout.take().expect("refrain writes to its output"));
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .expect("the first snapshot must arrive");
    assert!(first.starts_with('{'), "NDJSON is expected");

    // The reader goes away: exactly what `head -1` does.
    drop(reader);

    let out = child.wait_with_output().expect("refrain must terminate");
    assert!(
        out.status.success(),
        "a closed pipe must return 0, not {} — {}",
        out.status,
        stderr(&out)
    );
    assert_eq!(stderr(&out), "", "and say nothing on standard error");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_json_stream_closes_the_last_requests_of_a_quiet_source() {
    // Durations measured by correlation close on the log clock: a request
    // ends when no line has carried its token for `--correlate-timeout`. On a
    // quiet source no line comes to advance that clock, and the stream used to
    // report the same open requests, with no latency, snapshot after snapshot.
    let dir = workdir("quiet");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "3",
        "--no-durations",
        "--seed",
        "3",
        path,
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let mut child = Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args([
            "--json",
            "--every",
            "0.2",
            "--correlate-timeout",
            "0.2",
            "--from-start",
            path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("refrain must be able to start");

    let mut reader = BufReader::new(child.stdout.take().expect("refrain writes to its output"));
    let mut settled = None;
    // Twenty snapshots — four seconds — is far more than the timeout needs.
    for _ in 0..20 {
        let mut line = String::new();
        reader.read_line(&mut line).expect("a snapshot must arrive");
        let snapshot: Value = serde_json::from_str(&line).expect("NDJSON is expected");
        if snapshot["open_requests"] == 0 {
            settled = Some(snapshot);
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let snapshot = settled.expect("the three requests must close once the source is quiet");
    assert_eq!(snapshot["duration_source"]["kind"], "correlation");
    let timed: u64 = snapshot["endpoints"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|endpoint| endpoint["timed"].as_u64().unwrap())
        .sum();
    assert_eq!(timed, 3, "each request measured once: {snapshot}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn standard_input_can_be_analysed() {
    // `ssh prod tail -f … | refrain -`: the pipe must work like a file. So the
    // two processes are really plugged into each other — going through an
    // intermediate file would check everything except the "-" path.
    let mut source = Command::new(env!("CARGO_BIN_EXE_genlogs"))
        .args(["--rate", "0", "--count", "50", "--seed", "5"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("genlogs must be able to start");
    let pipe = source.stdout.take().expect("genlogs writes to its output");

    let out = Command::new(env!("CARGO_BIN_EXE_refrain"))
        .args(["--json", "-"])
        .stdin(Stdio::from(pipe))
        .output()
        .expect("refrain must be able to start");

    let tail = source.wait().expect("genlogs must terminate");
    assert!(tail.success(), "genlogs failed");
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));

    let report: Value = serde_json::from_slice(&out.stdout).expect("JSON valide");
    assert!(report["totals"]["entries"].as_u64().unwrap() > 200);
    assert_eq!(report["duration_source"]["kind"], "field");
    assert_eq!(
        report["endpoints"].as_array().unwrap().len(),
        8,
        "the generator's eight routes must come out of the pipe"
    );
}

#[test]
fn n_restricts_the_report_to_the_end_of_the_file() {
    // On a multi-gigabyte `prod.log`, "summarise the end for me" must really
    // read the end only: `-n` used to be silently ignored in the report modes,
    // which reread the whole file.
    let dir = workdir("dernieres-lignes");
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let out = genlogs(&["--rate", "0", "--count", "400", "--seed", "9", path]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    let entries = |args: &[&str]| -> u64 {
        let out = refrain(args);
        assert!(out.status.success(), "refrain failed: {}", stderr(&out));
        let report: Value = serde_json::from_slice(&out.stdout).expect("JSON valide");
        report["totals"]["entries"].as_u64().unwrap()
    };

    let whole = entries(&["--json", path]);
    let tail = entries(&["--json", "-n", "500", path]);
    assert!(whole > 4000, "the whole file is far bigger: {whole}");
    // One entry per line, except stack traces glued to the previous one.
    assert!(tail <= 500, "only the last 500 lines: {tail}");
    assert!(tail > 400, "but a full 500, not a handful: {tail}");
    // Larger than the file: we fall back on the whole of it.
    assert_eq!(entries(&["--json", "-n", "999999", path]), whole);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_source_fails_the_command() {
    // The monitoring trap: without this, a cron job on a faulty path would get
    // a zeroed snapshot and exit code 0, hence "all is well".
    let out = refrain(&["--json", "/introuvable/prod.log"]);
    assert!(
        !out.status.success(),
        "a missing file must produce a non-zero exit code"
    );
    assert!(
        stderr(&out).contains("introuvable"),
        "the message must name the faulty file: {}",
        stderr(&out)
    );
}

#[test]
fn incompatible_options_are_refused() {
    assert!(!refrain(&["--json", "--summary", "x.log"]).status.success());
    assert!(!refrain(&["--every", "5", "x.log"]).status.success());
    // "from the start" and "the last N lines" contradict each other.
    assert!(!refrain(&["-a", "-n", "10", "x.log"]).status.success());
}

#[test]
fn the_time_window_restricts_the_report() {
    let dir = workdir("fenetre");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");

    // A file with known dates rather than `genlogs` output: we want to be able
    // to say exactly what must fall on either side of the bounds.
    let mut content = String::new();
    for (heure, route) in [
        ("09:59:59", "avant_la_fenetre"),
        ("10:00:00", "dans_la_fenetre"),
        ("10:30:00", "dans_la_fenetre_aussi"),
        ("11:00:00", "apres_la_fenetre"),
    ] {
        content.push_str(&format!(
            "[2026-09-09T{heure}.000000+02:00] request.INFO: Matched route \"{route}\". \
             {{\"route\":\"{route}\"}} []\n"
        ));
    }
    std::fs::write(&log, content).unwrap();
    let path = log.to_str().unwrap();

    // Without a window: all four lines.
    let out = refrain(&["--json", path]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["totals"]["entries"], 4);
    assert_eq!(report["totals"]["out_of_window"], 0);

    // With a window: the two in the middle, and the other two counted apart —
    // above all not in `skipped`, which signals a format problem.
    let out = refrain(&[
        "--json",
        "--since",
        "2026-09-09T10:00:00+02:00",
        "--until",
        "2026-09-09T10:30:00+02:00",
        path,
    ]);
    assert!(out.status.success(), "refrain failed: {}", stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["totals"]["entries"], 2, "only the middle lines");
    assert_eq!(report["totals"]["out_of_window"], 2);
    assert_eq!(report["totals"]["skipped"], 0);

    // And the endpoints outside the window have indeed left the aggregates.
    let endpoints = report["endpoints"].as_array().unwrap();
    let noms: Vec<&str> = endpoints
        .iter()
        .map(|e| e["endpoint"].as_str().unwrap())
        .collect();
    assert!(noms.contains(&"dans_la_fenetre"), "{noms:?}");
    assert!(!noms.contains(&"avant_la_fenetre"), "{noms:?}");
    assert!(!noms.contains(&"apres_la_fenetre"), "{noms:?}");

    // The text summary says what was dropped.
    let out = refrain(&["--summary", "--since", "2026-09-09T10:00:00+02:00", path]);
    let texte = String::from_utf8_lossy(&out.stdout);
    assert!(texte.contains("outside the bounds"), "{texte}");

    // A malformed bound is refused at start-up, not after reading.
    let out = refrain(&["--summary", "--since", "hier matin", path]);
    assert!(!out.status.success(), "an absurd bound must be refused");
    assert!(
        stderr(&out).contains("neither a duration"),
        "{}",
        stderr(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The CI use of the N+1 detector: the test suite runs with Doctrine logging
/// on, and refrain fails the build on what it finds.
#[test]
fn an_nplus1_threshold_fails_the_build() {
    let dir = workdir("nplus1");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("test.log");

    // One request, the same prepared statement twelve times: one N+1 pattern
    // on app_orders, none on app_home.
    let mut content = String::new();
    for (token, route, repeats) in [("aaa", "app_orders", 12), ("bbb", "app_home", 1)] {
        content.push_str(&format!(
            "[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route \"{route}\". \
             {{\"route\":\"{route}\"}} {{\"token\":\"{token}\"}}\n"
        ));
        for _ in 0..repeats {
            content.push_str(&format!(
                "[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement \
                 {{\"sql\":\"SELECT t0.id FROM customer t0 WHERE t0.id = ?\",\"params\":{{\"1\":1}}}} \
                 {{\"token\":\"{token}\"}}\n"
            ));
        }
        content.push_str(&format!(
            "[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished \
             {{\"route\":\"{route}\",\"status\":200,\"duration_ms\":120.0}} {{\"token\":\"{token}\"}}\n"
        ));
    }
    std::fs::write(&log, content).unwrap();
    let path = log.to_str().unwrap();

    // The build fails, and the message says which route.
    let out = refrain(&["--summary", "--fail-if", "nplus1>0", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("nplus1 = 1 > 0"), "{}", stderr(&out));
    let out = refrain(&["--summary", "--fail-if", "nplus1:app_orders>0", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nplus1 (app_orders) = 1 > 0"),
        "{}",
        stderr(&out)
    );

    // The clean route passes.
    let out = refrain(&["--summary", "--fail-if", "nplus1:app_home>0", path]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // A higher detection threshold, and the twelve executions are no longer
    // an N+1: the two options agree with each other.
    let out = refrain(&["--summary", "--nplus1", "20", "--fail-if", "nplus1>0", path]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // Detection switched off, the threshold could never be crossed: refused
    // at start-up as a faulty command line, not passed as a green build.
    let out = refrain(&["--summary", "--nplus1", "0", "--fail-if", "nplus1>0", path]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--nplus1 0"), "{}", stderr(&out));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_thresholds_decide_the_exit_code() {
    let dir = workdir("seuils");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");

    // Four requests with known durations, one of them in error: error rate of
    // 25 %, worst duration 3 s.
    let mut content = String::new();
    for (route, ms) in [("lent", 3000.0), ("rapide", 20.0), ("rapide", 30.0)] {
        content.push_str(&format!(
            "[2026-09-09T10:00:00.000000+02:00] request.INFO: Request finished \
             {{\"route\":\"{route}\",\"duration_ms\":{ms}}} []\n"
        ));
    }
    content.push_str(
        "[2026-09-09T10:00:01.000000+02:00] request.CRITICAL: Uncaught PHP Exception \
         App\\Exception\\Boom: \"nope\" at /var/www/src/X.php line 12 \
         {\"route\":\"lent\"} []\n",
    );
    std::fs::write(&log, content).unwrap();
    let path = log.to_str().unwrap();

    // Threshold respected: 0.
    let out = refrain(&["--summary", "--fail-if", "error-rate>50%", path]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // Threshold crossed: 3, distinct from the 1 of unreadable sources as from
    // the 2 clap returns for a faulty command line.
    let out = refrain(&["--summary", "--fail-if", "error-rate>10%", path]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("threshold crossed"),
        "{}",
        stderr(&out)
    );
    // The report stays on standard output: a pipe downstream is not polluted.
    assert!(String::from_utf8_lossy(&out.stdout).contains("summary"));

    // Several thresholds, one of them on the worst endpoint, which must be named.
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "error-rate>10%",
        "--fail-if",
        "p95>1s",
        path,
    ]);
    assert_eq!(out.status.code(), Some(3));
    let failures = stderr(&out);
    assert_eq!(
        failures.lines().count(),
        2,
        "one crossed threshold per line: {failures}"
    );
    assert!(failures.contains("p95 (lent)"), "{failures}");

    // An unreadable source takes precedence: the figures mean nothing.
    let out = refrain(&[
        "--summary",
        "--fail-if",
        "error-rate>10%",
        "/introuvable.log",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    // A malformed threshold is refused at start-up, before any reading — and
    // with 2, which sets it apart from a threshold genuinely crossed.
    let out = refrain(&["--summary", "--fail-if", "p95 is too large", path]);
    assert_eq!(out.status.code(), Some(2), "a faulty command line");
    assert!(
        stderr(&out).contains("has no comparator"),
        "{}",
        stderr(&out)
    );

    // And a threshold makes no sense without a report that ends.
    let out = refrain(&["--fail-if", "error-rate>10%", path]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("one-shot report"), "{}", stderr(&out));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rotated_log_gives_the_same_result_as_a_plain_one() {
    let dir = workdir("gzip");
    std::fs::create_dir_all(&dir).unwrap();
    let plain = dir.join("prod.log");

    let out = genlogs(&[
        "--rate",
        "0",
        "--count",
        "200",
        "--seed",
        "3",
        plain.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "genlogs failed: {}", stderr(&out));

    // Compressed by the real `gzip`, the way logrotate would — and not by the
    // library used to read it back: we want to know we can read what the system
    // produces, not only what we produce ourselves.
    let copie = dir.join("prod.log.1");
    std::fs::copy(&plain, &copie).unwrap();
    let gzip = Command::new("gzip")
        .arg(&copie)
        .status()
        .expect("gzip must be installed");
    assert!(gzip.success(), "gzip failed");
    let compressed = dir.join("prod.log.1.gz");
    assert!(compressed.exists());
    assert!(
        std::fs::metadata(&compressed).unwrap().len() < std::fs::metadata(&plain).unwrap().len(),
        "the compressed file must be smaller"
    );

    let read_json = |path: &std::path::Path| -> Value {
        let out = refrain(&["--json", "--top", "0", path.to_str().unwrap()]);
        assert!(out.status.success(), "refrain failed: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).expect("some JSON")
    };

    let expected = read_json(&plain);
    let got = read_json(&compressed);

    assert!(
        expected["totals"]["entries"].as_u64().unwrap() > 1000,
        "the test file must be substantial"
    );
    assert_eq!(got["totals"], expected["totals"], "same totals");
    assert_eq!(got["levels"], expected["levels"], "same levels");
    assert_eq!(got["endpoints"], expected["endpoints"], "same endpoints");
    assert_eq!(got["nplus1"], expected["nplus1"], "same N+1 patterns");

    // Both together: that is the post-mortem gesture, yesterday and today
    // handed over at once.
    let out = refrain(&[
        "--json",
        plain.to_str().unwrap(),
        compressed.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let ensemble: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        ensemble["totals"]["entries"].as_u64().unwrap(),
        expected["totals"]["entries"].as_u64().unwrap() * 2,
        "both sources must be counted"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn genlogs_spreads_the_requests_over_time() {
    let dir = workdir("spread");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("prod.log");
    let path = log.to_str().unwrap();

    let span = |args: &[&str]| -> f64 {
        let out = genlogs(args);
        assert!(out.status.success(), "genlogs failed: {}", stderr(&out));
        let out = refrain(&["--json", path]);
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        let span = report["window"]["span_seconds"].as_f64().unwrap();
        std::fs::remove_file(&log).unwrap();
        span
    };

    // Without spreading, everything is written within a few milliseconds:
    // refrain's graphs would shrink to a single bar.
    let tight = span(&["--rate", "0", "--count", "200", "--seed", "5", path]);
    assert!(
        tight < 5.0,
        "without --spread, the window must be narrow: {tight}"
    );

    // With it, the requests cover the window asked for.
    let spread = span(&[
        "--rate", "0", "--count", "200", "--spread", "60", "--seed", "5", path,
    ]);
    assert!(
        (55.0..=62.0).contains(&spread),
        "--spread 60 must cover a minute, not {spread} s"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
