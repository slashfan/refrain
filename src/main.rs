//! ruru — analyseur de logs Symfony/Monolog en temps réel.
//!
//! Architecture générale :
//!
//! ```text
//!   thread(s) tail ─┐
//!   thread clavier ─┼──► canal mpsc ──► boucle principale ──► ratatui
//!   thread horloge ─┘                    (App: décide)        (ui: dessine)
//! ```
//!
//! Un seul thread touche à l'état de l'application, ce qui évite tout verrou :
//! la concurrence passe uniquement par le canal.

mod app;
mod cli;
mod event;
mod export;
mod parser;
mod stats;
mod tail;
mod ui;

use anyhow::{Context, Result};
use app::App;
use clap::Parser;
use cli::{Cli, Mode};
use event::Event;
use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

/// Nombre d'événements avalés d'affilée avant de redessiner. Sans ce plafond,
/// un fichier de 40 Go monopoliserait la boucle et l'écran resterait figé.
const MAX_DRAIN: usize = 2048;

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.mode() {
        Mode::Tui => run_tui(cli),
        Mode::Summary => run_report(cli, Report::Text),
        Mode::JsonOnce => run_report(cli, Report::Json),
        Mode::JsonStream => run_json_stream(cli),
    }
}

/// Format d'un rapport ponctuel.
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
    // Le `tx` d'origine est inutile désormais : chaque thread a son clone.
    drop(tx);

    let sources = cli.files.len();
    let mut app = App::new(cli, sources);
    let mut ui_state = ui::UiState::default();

    // `init` passe le terminal en mode brut, bascule sur l'écran alternatif et
    // installe un hook de panique qui restaure tout : même en cas de bug, on ne
    // laisse pas le terminal dans un état inutilisable.
    let mut terminal = ratatui::init();

    let outcome = (|| -> Result<()> {
        terminal.draw(|frame| ui::draw(frame, &app, &mut ui_state))?;

        while let Ok(first) = rx.recv() {
            let mut redraw = app.on_event(first);

            // On vide ce qui est déjà arrivé pendant qu'on dessinait : mille
            // petits lots traités d'un coup coûtent bien moins cher que mille
            // rendus successifs.
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
                    .context("échec du rendu")?;
            }
        }
        Ok(())
    })();

    ratatui::restore();

    for failure in &app.failures {
        eprintln!("ruru: {failure}");
    }
    outcome?;
    Ok(exit_code(&app))
}

/// Une source illisible doit se voir jusque dans le code de sortie : sans ça,
/// un `--json` en cron sur un chemin fautif rendrait un instantané à zéro que
/// le collecteur prendrait pour « tout va bien ».
fn exit_code(app: &App) -> ExitCode {
    if app.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Modes `--summary` et `--json` : pas d'interface, pas de suivi. On lit les
/// fichiers en entier, puis on écrit un rapport sur la sortie standard.
fn run_report(cli: Cli, report: Report) -> Result<ExitCode> {
    let (tx, rx) = mpsc::channel();
    let options = tail_options(&cli);
    let sources = cli.files.len();

    for (source, path) in cli.files.iter().enumerate() {
        tail::spawn(source, path.clone(), options.clone(), tx.clone());
    }
    // Indispensable ici : tant qu'un `Sender` existe, `recv()` attend. En le
    // relâchant, la boucle s'arrête d'elle-même quand tous les threads ont fini.
    drop(tx);

    let top = cli.top;
    let mut app = App::new(cli, sources);
    while let Ok(event) = rx.recv() {
        app.on_event(event);
    }
    app.stats.finalize();

    for failure in &app.failures {
        eprintln!("ruru: {failure}");
    }
    match report {
        Report::Text => print!("{}", stats::render_summary(&app.stats)),
        Report::Json => println!("{}", stats::render_json(&app.stats, top, true)),
    }
    Ok(exit_code(&app))
}

/// Mode `--json --every N` : on reste accroché aux fichiers et on émet un objet
/// JSON par intervalle, un par ligne. C'est du NDJSON, digeste tel quel pour
/// Vector, Fluent Bit ou un collecteur maison :
///
/// ```bash
/// ruru --json --every 30 var/log/prod.log | while read -r line; do …; done
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
    // On verrouille la sortie une fois pour toutes plutôt qu'à chaque écriture.
    let mut out = std::io::stdout().lock();

    while let Ok(event) = rx.recv() {
        match event {
            Event::Tick => emit(&mut out, &app, top)?,
            other => {
                let source_ended = matches!(other, Event::SourceDone(_));
                app.on_event(other);
                // Une source finie pour de bon (stdin fermée) : dernier
                // instantané, puis on s'arrête au lieu de tourner à vide.
                if source_ended && app.all_sources_done() {
                    app.stats.finalize();
                    emit(&mut out, &app, top)?;
                    break;
                }
            }
        }
    }

    for failure in &app.failures {
        eprintln!("ruru: {failure}");
    }
    Ok(exit_code(&app))
}

fn emit(out: &mut impl Write, app: &App, top: usize) -> Result<()> {
    writeln!(out, "{}", stats::render_json(&app.stats, top, false))?;
    out.flush().context("écriture de l'instantané JSON")?;
    Ok(())
}
