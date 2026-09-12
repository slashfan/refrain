//! The binary: thread wiring, main loop, exit codes.
//!
//! Everything else lives in the library (`src/lib.rs`), so the benchmark can
//! call its functions directly.

use anyhow::{Context, Result};
use clap::Parser;
use refrain::app::App;
use refrain::cli::{Cli, Mode};
use refrain::event::Event;
use refrain::{event, stats, tail, ui};
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

/// Number of events swallowed in a row before redrawing. Without this cap, a
/// 40 GB file would monopolise the loop and the screen would stay frozen.
const MAX_DRAIN: usize = 2048;

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // `--every` is already refused by clap; the dashboard, itself, has no
    // option to express it: a threshold only makes sense on a report that ends
    // and returns an exit code.
    if !cli.fail_if.is_empty() && cli.mode() == Mode::Tui {
        anyhow::bail!("--fail-if needs a one-shot report: add --summary or --json");
    }
    match cli.mode() {
        Mode::Tui => run_tui(cli),
        Mode::Summary => run_report(cli, Report::Text),
        Mode::JsonOnce => run_report(cli, Report::Json),
        Mode::JsonStream => run_json_stream(cli),
    }
}

/// Format of a one-shot report.
#[derive(Clone, Copy)]
enum Report {
    Text,
    Json,
}

fn tail_options(cli: &Cli) -> tail::Options {
    tail::Options {
        from_start: cli.read_from_start(),
        lines: cli.lines,
        follow: cli.follow(),
        poll: Duration::from_millis(100),
    }
}

fn run_tui(cli: Cli) -> Result<ExitCode> {
    let (tx, rx) = mpsc::channel();
    let options = tail_options(&cli);

    for (source, path) in cli.files.iter().enumerate() {
        tail::spawn(source, path.clone(), options.clone(), tx.clone());
    }
    event::spawn_input(tx.clone());
    event::spawn_ticker(tx.clone(), Duration::from_millis(cli.tick_ms.max(30)));
    // The original `tx` is useless now: each thread has its own clone.
    drop(tx);

    let sources = cli.files.len();
    let mut app = App::new(cli, sources);
    let mut ui_state = ui::UiState::default();

    // `init` puts the terminal in raw mode, switches to the alternate screen
    // and installs a panic hook that restores everything: even on a bug, the
    // terminal is not left unusable.
    let mut terminal = ratatui::init();

    let outcome = (|| -> Result<()> {
        terminal.draw(|frame| ui::draw(frame, &app, &mut ui_state))?;

        while let Ok(first) = rx.recv() {
            let mut redraw = app.on_event(first);

            // Drain whatever arrived while we were drawing: a thousand small
            // batches handled at once cost far less than a thousand successive
            // renders.
            for _ in 0..MAX_DRAIN {
                match rx.try_recv() {
                    Ok(next) => redraw |= app.on_event(next),
                    Err(_) => break,
                }
            }

            if app.should_quit {
                break;
            }
            if redraw {
                terminal
                    .draw(|frame| ui::draw(frame, &app, &mut ui_state))
                    .context("draw failed")?;
            }
        }
        Ok(())
    })();

    ratatui::restore();

    for failure in &app.failures {
        eprintln!("refrain: {failure}");
    }
    outcome?;
    Ok(exit_code(&app))
}

/// An unreadable source must show all the way into the exit code: without
/// that, a `--json` in cron on a faulty path would return a zeroed snapshot the
/// collector would take for "all is well".
fn exit_code(app: &App) -> ExitCode {
    if app.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Exit codes of a report:
///
/// | Code | Cause |
/// | --- | --- |
/// | 0 | all is well |
/// | 1 | a source could not be read |
/// | 2 | the command line is at fault (clap) |
/// | 3 | a `--fail-if` threshold was crossed |
///
/// The issue asked for 2 on a crossed threshold, but clap already returns that
/// for an invalid argument — a malformed threshold and a crossed one would
/// then have been indistinguishable to a job, which would take a typo for an
/// application in distress. Hence 3.
///
/// The unreadable source takes precedence over the threshold: if not everything
/// was read, the figures underpinning it mean nothing, and a job must be able
/// to tell "the application is unwell" from "refrain could not read anything".
fn report_exit_code(app: &App, breaches: usize) -> ExitCode {
    if !app.failures.is_empty() {
        ExitCode::FAILURE
    } else if breaches > 0 {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}

/// Modes `--summary` and `--json`: no interface, no following. The files are
/// read in full, then a report is written to standard output.
fn run_report(cli: Cli, report: Report) -> Result<ExitCode> {
    let (tx, rx) = mpsc::channel();
    let options = tail_options(&cli);
    let sources = cli.files.len();

    for (source, path) in cli.files.iter().enumerate() {
        tail::spawn(source, path.clone(), options.clone(), tx.clone());
    }
    // Indispensable here: as long as a `Sender` exists, `recv()` waits. By
    // releasing it, the loop ends by itself once every thread has finished.
    drop(tx);

    let top = cli.top;
    let thresholds = cli.fail_if.clone();
    let mut app = App::new(cli, sources);
    while let Ok(event) = rx.recv() {
        app.on_event(event);
    }
    app.stats.finalize();

    for failure in &app.failures {
        eprintln!("refrain: {failure}");
    }
    let report = match report {
        Report::Text => stats::render_summary(&app.stats),
        Report::Json => format!("{}\n", stats::render_json(&app.stats, top, true)),
    };
    // `print!` **panics** if the write fails. On a closed pipe that is not a
    // failure, and the thresholds are evaluated anyway: their verdict does not
    // depend on who reads the report.
    write_out(&mut io::stdout().lock(), &report)?;

    // The thresholds are evaluated once everything is read, and go to standard
    // error: the report itself stays usable through a pipe.
    let breaches: Vec<_> = thresholds
        .iter()
        .filter_map(|seuil| seuil.check(&app.stats))
        .collect();
    for breach in &breaches {
        eprintln!("refrain: threshold crossed — {breach}");
    }
    Ok(report_exit_code(&app, breaches.len()))
}

/// Mode `--json --every N`: we stay attached to the files and emit one JSON
/// object per interval, one per line. That is NDJSON, digestible as it stands
/// by Vector, Fluent Bit or a home-grown collector:
///
/// ```bash
/// refrain --json --every 30 var/log/prod.log | while read -r line; do …; done
/// ```
fn run_json_stream(cli: Cli) -> Result<ExitCode> {
    let (tx, rx) = mpsc::channel();
    let options = tail_options(&cli);
    let sources = cli.files.len();
    let top = cli.top;
    let period = cli.snapshot_period();

    for (source, path) in cli.files.iter().enumerate() {
        tail::spawn(source, path.clone(), options.clone(), tx.clone());
    }
    event::spawn_ticker(tx.clone(), period);
    drop(tx);

    let mut app = App::new(cli, sources);
    // The output is locked once and for all rather than on every write.
    let mut out = std::io::stdout().lock();

    while let Ok(event) = rx.recv() {
        match event {
            Event::Tick => {
                // What the dashboard does on every tick: without it, on a
                // quiet source, the last requests measured by correlation
                // stayed open for ever — a collector polling a quiet site saw
                // requests that never ended and no latency at all.
                app.stats.sweep_idle();
                // No reader left: there is no point following the files for
                // an output nobody will read.
                if !emit(&mut out, &app, top)? {
                    break;
                }
            }
            other => {
                let source_ended = matches!(other, Event::SourceDone(_));
                app.on_event(other);
                // A source finished for good (stdin closed): a last snapshot,
                // then we stop instead of spinning on nothing.
                if source_ended && app.all_sources_done() {
                    app.stats.finalize();
                    emit(&mut out, &app, top)?;
                    break;
                }
            }
        }
    }

    for failure in &app.failures {
        eprintln!("refrain: {failure}");
    }
    Ok(exit_code(&app))
}

fn emit(out: &mut impl Write, app: &App, top: usize) -> Result<bool> {
    write_out(
        out,
        &format!("{}\n", stats::render_json(&app.stats, top, false)),
    )
}

/// Writes to standard output, and tells a closed pipe from a real failure.
///
/// `Ok(false)`: there is nobody left at the other end — a `| head` that has had
/// its fill, a collector that restarted. This is not one more error to report
/// but the end of a reader, and confusing it with a failure would cost dearly:
/// exit code 1 announces "a source could not be read", and a cron job would
/// believe the logs unreadable when they were read in full.
fn write_out(out: &mut impl Write, text: &str) -> Result<bool> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err).context("writing to standard output"),
    }
}
