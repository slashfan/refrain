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
