//! Le binaire : câblage des threads, boucle principale, codes de sortie.
//!
//! Tout le reste vit dans la bibliothèque (`src/lib.rs`), pour que le banc de
//! mesure puisse en appeler les fonctions directement.

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

/// Nombre d'événements avalés d'affilée avant de redessiner. Sans ce plafond,
/// un fichier de 40 Go monopoliserait la boucle et l'écran resterait figé.
const MAX_DRAIN: usize = 2048;

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // `--every` est déjà refusé par clap ; le tableau de bord, lui, n'a pas
    // d'options pour l'exprimer : un seuil n'a de sens que sur un rapport qui
    // se termine et rend un code de sortie.
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

/// Codes de sortie d'un rapport :
///
/// | Code | Cause |
/// | --- | --- |
/// | 0 | tout va bien |
/// | 1 | une source n'a pas pu être lue |
/// | 2 | la ligne de commande est fautive (clap) |
/// | 3 | un seuil `--fail-if` est franchi |
///
/// Le ticket demandait 2 pour un seuil franchi, mais clap le rend déjà pour un
/// argument invalide — un seuil mal écrit et un seuil franchi auraient alors
/// été indiscernables par un job, qui aurait pris une faute de frappe pour une
/// application en détresse. D'où 3.
///
/// La source illisible prime sur le seuil : si l'on n'a pas tout lu, les
/// chiffres qui le sous-tendent ne veulent rien dire, et un job doit pouvoir
/// distinguer « l'application va mal » de « refrain n'a rien pu lire ».
fn report_exit_code(app: &App, breaches: usize) -> ExitCode {
    if !app.failures.is_empty() {
        ExitCode::FAILURE
    } else if breaches > 0 {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
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
    let seuils = cli.fail_if.clone();
    let mut app = App::new(cli, sources);
    while let Ok(event) = rx.recv() {
        app.on_event(event);
    }
    app.stats.finalize();

    for failure in &app.failures {
        eprintln!("refrain: {failure}");
    }
    let rapport = match report {
        Report::Text => stats::render_summary(&app.stats),
        Report::Json => format!("{}\n", stats::render_json(&app.stats, top, true)),
    };
    // `print!` **panique** si l'écriture échoue. Sur un tube fermé, ce n'est pas
    // une panne, et les seuils s'évaluent de toute façon : leur verdict ne
    // dépend pas de qui lit le rapport.
    write_out(&mut io::stdout().lock(), &rapport)?;

    // Les seuils s'évaluent une fois tout lu, et se disent sur la sortie
    // d'erreur : le rapport lui-même reste exploitable par un tube.
    let mut scratch = Vec::new();
    let breaches: Vec<_> = seuils
        .iter()
        .filter_map(|seuil| seuil.check(&app.stats, &mut scratch))
        .collect();
    for breach in &breaches {
        eprintln!("refrain: threshold crossed — {breach}");
    }
    Ok(report_exit_code(&app, breaches.len()))
}

/// Mode `--json --every N` : on reste accroché aux fichiers et on émet un objet
/// JSON par intervalle, un par ligne. C'est du NDJSON, digeste tel quel pour
/// Vector, Fluent Bit ou un collecteur maison :
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
    // On verrouille la sortie une fois pour toutes plutôt qu'à chaque écriture.
    let mut out = std::io::stdout().lock();

    while let Ok(event) = rx.recv() {
        match event {
            // Plus de lecteur : rien ne sert de suivre les fichiers pour une
            // sortie que personne ne lira.
            Event::Tick => {
                if !emit(&mut out, &app, top)? {
                    break;
                }
            }
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

/// Écrit sur la sortie standard, et distingue le tube fermé d'une vraie panne.
///
/// `Ok(false)` : il n'y a plus personne à l'autre bout — un `| head` qui a eu
/// son compte, un collecteur qui a redémarré. Ce n'est pas une erreur de plus à
/// signaler mais une fin de lecteur, et la confondre avec une panne coûterait
/// cher : le code 1 annonce « une source n'a pas pu être lue », et un cron
/// croirait les journaux illisibles alors qu'ils ont été lus entièrement.
fn write_out(out: &mut impl Write, text: &str) -> Result<bool> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err).context("writing to standard output"),
    }
}
