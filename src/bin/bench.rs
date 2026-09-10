//! Banc de mesure : combien de lignes par seconde, et où passe le temps.
//!
//! Le README annonce un débit en tête de page. Ce chiffre venait d'une mesure
//! ponctuelle, un jour, sur une machine ; rien ne le rejouait, et rien n'aurait
//! signalé qu'un changement le divise par trois. Ce banc le rejoue à la
//! demande, sur un corpus à graine fixe, et sert de garde-fou en CI.
//!
//! ```bash
//! cargo run --release --bin bench                    # corpus engendré
//! cargo run --release --bin bench -- var/log/prod.log
//! cargo run --release --bin bench -- --min 100000    # échoue en deçà
//! ```
//!
//! Deux mesures, parce qu'une seule ne dirait pas où passe le temps :
//! l'analyse d'une ligne, puis l'analyse **et** l'agrégation. La lecture du
//! fichier est délibérément hors chronomètre — c'est le processeur qu'on
//! mesure ici, pas le disque.

use anyhow::{Context, Result, bail};
use clap::Parser as _;
use refrain::cli::Cli;
use refrain::parser::parse_line;
use refrain::stats::Stats;
use refrain::stats::format_count;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

/// Nombre de requêtes simulées quand le banc engendre son propre corpus.
/// Environ 1,15 million de lignes : assez pour que la mesure ne dépende plus
/// du bruit de fond.
const REQUETES: usize = 100_000;
/// Chaque passe est rejouée, et c'est la meilleure qui compte : la plus lente
/// mesure surtout ce que la machine faisait d'autre au même moment.
const PASSES: usize = 3;
/// Graine du corpus engendré : fixe, pour que deux exécutions se comparent.
const GRAINE: u64 = 1;

fn main() -> Result<()> {
    let mut chemin: Option<PathBuf> = None;
    let mut minimum: Option<f64> = None;
    let mut requetes = REQUETES;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--min" => {
                let valeur = args.next().context("--min attend un nombre de lignes/s")?;
                minimum = Some(valeur.parse().context("--min attend un nombre")?);
            }
            "--requetes" => {
                let valeur = args.next().context("--requetes attend un nombre")?;
                requetes = valeur.parse().context("--requetes attend un nombre")?;
            }
            "-h" | "--help" => {
                println!("banc de mesure de refrain\n");
                println!("  bench [FICHIER] [--min LIGNES_PAR_SECONDE] [--requetes N]\n");
                println!("Sans fichier, un corpus est engendré par genlogs (graine fixe).");
                println!("--requetes règle la taille de ce corpus [{REQUETES}].");
                return Ok(());
            }
            autre => chemin = Some(PathBuf::from(autre)),
        }
    }

    let (chemin, engendre) = match chemin {
        Some(chemin) => (chemin, false),
        None => (engendrer(requetes)?, true),
    };

    // Hors chronomètre, volontairement : on mesure l'analyse, pas le disque.
    let contenu = std::fs::read_to_string(&chemin)
        .with_context(|| format!("lecture de {}", chemin.display()))?;
    let lignes: Vec<&str> = contenu.lines().collect();
    if lignes.is_empty() {
        bail!("{} est vide", chemin.display());
    }

    println!(
        "corpus    : {} lignes, {:.1} Mo — {}",
        format_count(lignes.len() as u64),
        contenu.len() as f64 / 1_048_576.0,
        chemin.display()
    );
    if engendre {
        println!("            (graine fixe : deux exécutions se comparent)");
    }

    let parseur = mesurer(PASSES, || {
        let mut analysees = 0u64;
        for ligne in &lignes {
            if parse_line(ligne).is_some() {
                analysees += 1;
            }
        }
        analysees
    });
    afficher("parseur", lignes.len(), parseur);

    // Un `Cli` par défaut : corrélation active, détection N+1 au seuil usuel.
    // C'est le chemin que suit réellement `refrain prod.log`.
    let modele = Cli::parse_from(["refrain", "bench.log"]);
    let complet = mesurer(PASSES, || {
        let mut stats = Stats::new(&modele);
        for ligne in &lignes {
            if let Some(entree) = parse_line(ligne) {
                stats.ingest(0, entree);
            }
        }
        stats.finalize();
        stats.total
    });
    afficher("+ agrégat", lignes.len(), complet);

    let debit = lignes.len() as f64 / complet;
    if let Some(minimum) = minimum
        && debit < minimum
    {
        bail!(
            "débit effondré : {} lignes/s, en deçà du plancher de {}",
            format_count(debit as u64),
            format_count(minimum as u64)
        );
    }
    Ok(())
}

/// Rejoue la passe et rend la meilleure durée, en secondes.
fn mesurer(passes: usize, mut passe: impl FnMut() -> u64) -> f64 {
    let mut meilleure = f64::MAX;
    for _ in 0..passes {
        let depart = Instant::now();
        let resultat = passe();
        let duree = depart.elapsed().as_secs_f64();
        // `black_box` empêcherait l'optimiseur de tout supprimer ; ici c'est le
        // résultat lui-même qu'on observe, ce qui suffit à le retenir.
        assert!(resultat > 0, "la passe n'a rien analysé");
        meilleure = meilleure.min(duree);
    }
    meilleure
}

fn afficher(quoi: &str, lignes: usize, secondes: f64) {
    println!(
        "{quoi:<10}: {:>12} lignes/s   ({:.0} ms)",
        format_count((lignes as f64 / secondes) as u64),
        secondes * 1000.0
    );
}

/// Engendre un corpus avec `genlogs`, qu'on va chercher à côté de soi : c'est
/// vrai sous `target/release` comme dans une archive de release.
fn engendrer(requetes: usize) -> Result<PathBuf> {
    let genlogs = std::env::current_exe()
        .context("chemin du banc")?
        .with_file_name(if cfg!(windows) {
            "genlogs.exe"
        } else {
            "genlogs"
        });
    if !genlogs.exists() {
        bail!(
            "{} est introuvable : compilez-le (cargo build --release) ou passez \
             un fichier de log en argument",
            genlogs.display()
        );
    }

    // Le nom porte les paramètres : changer la graine ou le volume donne un
    // autre fichier, plutôt qu'une réutilisation silencieuse du précédent.
    let chemin = std::env::temp_dir().join(format!("refrain-bench-{requetes}-g{GRAINE}.log"));
    // Le corpus est déterministe : le réengendrer à chaque exécution ne
    // changerait rien qu'à la patience de qui mesure.
    if chemin.exists() {
        return Ok(chemin);
    }
    let statut = Command::new(&genlogs)
        .args([
            "--rate",
            "0",
            "--count",
            &requetes.to_string(),
            "--seed",
            &GRAINE.to_string(),
        ])
        .arg(&chemin)
        .status()
        .with_context(|| format!("exécution de {}", genlogs.display()))?;
    if !statut.success() {
        bail!("genlogs a échoué");
    }
    Ok(chemin)
}
