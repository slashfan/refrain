//! Extraire l'élément sélectionné : le texte d'un rapport, son écriture dans un
//! fichier, sa copie dans le presse-papier.
//!
//! Quand on tient enfin l'erreur, on veut la coller dans un ticket. Retourner la
//! chercher à la main dans quarante gigaoctets de log annulerait le bénéfice de
//! l'avoir trouvée ici.

use crate::app::{App, Tab};
use crate::stats::{format_count, format_ms, format_time, render_summary};
use anyhow::{Context, Result};
use chrono::Local;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Un rapport prêt à être écrit ou collé.
pub struct Report {
    pub text: String,
    /// Nom de fichier lisible, sans extension :
    /// `ruru-erreur-ProductNotFound-20260909-231205`.
    pub slug: String,
}

/// Le rapport de l'élément sélectionné dans l'onglet courant.
///
/// Les onglets sans sélection — la vue d'ensemble, le flux — rendent le résumé
/// complet : `w` fait ainsi toujours quelque chose d'utile, plutôt que rien.
pub fn report(app: &App) -> Report {
    match app.tab {
        Tab::Errors => error_report(app),
        Tab::Endpoints => endpoint_report(app),
        Tab::Sql => nplus1_report(app),
        Tab::Overview | Tab::Stream => Report {
            text: with_header(app, "résumé", render_summary(&app.stats)),
            slug: slug("resume", None),
        },
    }
}

/// Écrit le rapport dans `dir` et rend son chemin.
pub fn write_to(app: &App, dir: &Path) -> Result<PathBuf> {
    let report = report(app);
    let path = dir.join(format!("{}.txt", report.slug));
    std::fs::write(&path, report.text.as_bytes())
        .with_context(|| format!("écriture de {}", path.display()))?;
    Ok(path)
}

/// La séquence OSC 52, qui demande au **terminal** de mettre ce texte dans le
/// presse-papier.
///
/// C'est le seul moyen qui traverse un `ssh` : le presse-papier visé est celui
/// de la machine où l'on regarde, pas celui du serveur où tourne ruru. Aucune
/// dépendance non plus, là où une bibliothèque de presse-papier tirerait X11 ou
/// Wayland sur Linux.
///
/// Tous les terminaux ne l'honorent pas — Terminal.app l'ignore, tmux le veut
/// avec `set -g set-clipboard on` — et certains plafonnent la taille de ce
/// qu'ils acceptent. D'où `w`, qui ne dépend de personne.
pub fn clipboard_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

// ---------------------------------------------------------------------------
// Les rapports
// ---------------------------------------------------------------------------

fn error_report(app: &App) -> Report {
    let Some(row) = app.error_rows.get(app.error_sel) else {
        return empty("erreur");
    };
    let Some(stat) = app.stats.errors.get(&row.signature) else {
        return empty("erreur");
    };

    let mut out = String::new();
    let _ = writeln!(out, "signature : {}", row.signature);
    let _ = writeln!(
        out,
        "vue       : {} fois, de {} à {}",
        format_count(stat.count),
        format_time(stat.first_seen),
        format_time(stat.last_seen)
    );
    let _ = writeln!(out, "niveau    : {}", stat.level.as_str());
    let _ = writeln!(out, "canal     : {}", stat.channel);
    if let Some(exception) = &stat.exception {
        let _ = writeln!(out, "exception : {exception}");
    }
    if let Some(endpoint) = &stat.endpoint {
        let _ = writeln!(out, "endpoint  : {endpoint}");
    }

    // Le message porte la trace d'exécution : les lignes de continuation lui
    // ont été rattachées à l'analyse. C'est tout l'intérêt de l'extraction —
    // l'écran, lui, n'en montre que les trois premières lignes.
    let _ = writeln!(out, "\nDernier exemplaire\n{}", stat.message);
    if let Some(context) = &stat.context {
        let _ = writeln!(out, "\nContexte\n{context}");
    }

    let nom = stat
        .exception
        .as_deref()
        .map(crate::parser::short_class)
        .unwrap_or(&stat.channel);
    Report {
        text: with_header(app, "erreur", out),
        slug: slug("erreur", Some(nom)),
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
        "requêtes : {} ({} en erreur, {:.1} %)",
        format_count(row.requests),
        format_count(row.errors),
        row.error_rate * 100.0
    );
    if row.timed > 0 {
        let _ = writeln!(
            out,
            "durées   : p50 {} · p95 {} · max {} (sur {} requêtes mesurées)",
            format_ms(row.p50),
            format_ms(row.p95),
            format_ms(row.max),
            format_count(row.timed)
        );
    } else {
        let _ = writeln!(out, "durées   : aucune mesurée");
    }
    if row.avg_queries > 0.0 {
        let _ = writeln!(out, "SQL/req  : {:.1} en moyenne", row.avg_queries);
    }
    let _ = writeln!(out, "mesure   : {}", app.stats.duration.label());

    // Les motifs N+1 de cet endpoint : c'est presque toujours l'explication
    // d'un p95 qui dérape, autant l'avoir dans le même presse-papier.
    let mut motifs: Vec<_> = app
        .stats
        .nplus1
        .values()
        .filter(|motif| motif.endpoint == row.name)
        .collect();
    motifs.sort_unstable_by_key(|motif| std::cmp::Reverse(motif.max_count));
    if !motifs.is_empty() {
        let _ = writeln!(out, "\nMotifs N+1 de cet endpoint");
        for motif in motifs.iter().take(10) {
            let _ = writeln!(
                out,
                "  {} × au pire, {:.1} en moyenne sur {} requêtes\n    {}",
                motif.max_count,
                motif.avg_count(),
                format_count(motif.requests),
                motif.sql
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
    let Some(motif) = app.stats.nplus1.get(&row.key) else {
        return empty("n+1");
    };

    let mut out = String::new();
    let _ = writeln!(out, "endpoint : {}", motif.endpoint);
    let _ = writeln!(
        out,
        "pire cas : {} exécutions dans une seule requête HTTP",
        motif.max_count
    );
    let _ = writeln!(
        out,
        "moyenne  : {:.1} sur {} requêtes HTTP touchées",
        motif.avg_count(),
        format_count(motif.requests)
    );
    let _ = writeln!(out, "dernière : {}", format_time(motif.last_seen));
    let _ = writeln!(out, "seuil    : {} ×", app.cli.nplus1);
    let _ = writeln!(out, "\nRequête répétée\n{}", motif.sql);

    Report {
        text: with_header(app, "motif N+1", out),
        slug: slug("nplus1", Some(&motif.endpoint)),
    }
}

fn empty(quoi: &str) -> Report {
    Report {
        text: format!("Rien à extraire : aucun {quoi} sélectionné.\n"),
        slug: slug(quoi, None),
    }
}

/// L'entête commun. Un rapport collé dans un ticket doit se suffire à
/// lui-même : ce qu'on regardait, quand, et d'où ça sort.
fn with_header(app: &App, quoi: &str, body: String) -> String {
    let sources: Vec<String> = app
        .cli
        .files
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    format!(
        "── ruru ─ {} ───────────────────────────────\n\
         extrait le {}\n\
         sources : {}\n\n{}",
        quoi,
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        sources.join(", "),
        body
    )
}

// ---------------------------------------------------------------------------
// Nom de fichier
// ---------------------------------------------------------------------------

fn slug(quoi: &str, nom: Option<&str>) -> String {
    let mut out = format!("ruru-{quoi}");
    if let Some(nom) = nom {
        let nom = sanitize(nom);
        if !nom.is_empty() {
            out.push('-');
            out.push_str(&nom);
        }
    }
    let _ = write!(out, "-{}", Local::now().format("%Y%m%d-%H%M%S"));
    out
}

/// Réduit un nom à ce qui passe partout dans un nom de fichier. Un endpoint est
/// souvent une URI (`/api/orders/42`), une exception un nom pleinement qualifié :
/// les deux portent des caractères dont on ne veut pas ici.
fn sanitize(nom: &str) -> String {
    let mut out = String::with_capacity(nom.len().min(40));
    let mut previous_dash = false;
    for c in nom.chars().take(60) {
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
// base64, pour OSC 52
// ---------------------------------------------------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodage base64 standard, avec remplissage. Une quinzaine de lignes contre
/// une dépendance de plus : le calcul est trivial et n'évoluera jamais.
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

    fn app_avec_une_erreur() -> App {
        let mut app = App::new(Cli::parse_from(["ruru", "var/log/prod.log"]), 1);
        let lignes = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_product_show". {"route":"app_product_show"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.100000+02:00] request.INFO: Request finished {"route":"app_product_show","duration_ms":120.0} {"token":"aaa"}"#,
        ];
        for ligne in lignes {
            app.stats
                .ingest(0, parse_line(ligne).expect("ligne valide"));
        }

        // Une exception avec sa trace d'exécution, telle que `tail.rs` la
        // remonte : les lignes de continuation rattachées au message.
        let ligne = r#"[2026-09-09T10:00:00.090000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\ProductNotFound: "Product 42 not found" at /var/www/src/Controller/ProductController.php line 88 {"exception":"[object] (App\\Exception\\ProductNotFound(code: 0): Product 42 not found at /var/www/src/Controller/ProductController.php:88)"} {"token":"aaa"}"#;
        let mut entry = parse_line(ligne).expect("ligne valide");
        entry.message.push_str(
            "\n#0 /var/www/src/Controller/ProductController.php(88): App\\Repository\\ProductRepository->find(42)\n#1 {main}",
        );
        app.stats.ingest(0, entry);

        app.stats.finalize();
        app.on_event(Event::Tick);
        app
    }

    #[test]
    fn le_rapport_d_erreur_porte_la_signature_et_sa_trace() {
        let mut app = app_avec_une_erreur();
        app.tab = Tab::Errors;
        let rapport = report(&app);

        assert!(rapport.text.contains("ProductNotFound"), "la signature");
        assert!(rapport.text.contains("app_product_show"), "l'endpoint");
        assert!(rapport.text.contains("CRITICAL"), "le niveau");
        assert!(rapport.text.contains("var/log/prod.log"), "la source");
        // L'écran n'en montre que trois lignes ; l'extraction, tout.
        assert!(
            rapport.text.contains("#0 /var/www/src/Controller"),
            "la trace d'exécution doit être complète"
        );
        assert!(
            rapport.text.contains("#1 {main}"),
            "jusqu'à sa dernière ligne"
        );
        assert!(rapport.slug.starts_with("ruru-erreur-ProductNotFound-"));
    }

    #[test]
    fn le_rapport_d_endpoint_porte_ses_chiffres() {
        let mut app = app_avec_une_erreur();
        app.tab = Tab::Endpoints;
        let rapport = report(&app);
        assert!(rapport.text.contains("app_product_show"));
        assert!(rapport.text.contains("120 ms"), "la durée mesurée");
        assert!(rapport.slug.starts_with("ruru-endpoint-app_product_show-"));
    }

    #[test]
    fn les_onglets_sans_selection_rendent_le_resume() {
        let mut app = app_avec_une_erreur();
        for tab in [Tab::Overview, Tab::Stream] {
            app.tab = tab;
            let rapport = report(&app);
            assert!(rapport.text.contains("résumé"));
            assert!(rapport.text.contains("Top erreurs"));
        }
    }

    #[test]
    fn le_rapport_s_ecrit_dans_un_fichier() {
        let mut app = app_avec_une_erreur();
        app.tab = Tab::Errors;

        let dir = std::env::temp_dir().join(format!("ruru-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let chemin = write_to(&app, &dir).expect("écriture");

        let contenu = std::fs::read_to_string(&chemin).unwrap();
        assert!(contenu.contains("ProductNotFound"), "la signature");
        assert!(contenu.contains("#1 {main}"), "la trace jusqu'au bout");

        let nom = chemin.file_name().unwrap().to_string_lossy();
        assert!(nom.starts_with("ruru-erreur-ProductNotFound-"), "{nom}");
        assert!(nom.ends_with(".txt"), "{nom}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base64_suit_la_reference() {
        // Les vecteurs de la RFC 4648.
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
        // Non-ASCII : c'est bien l'UTF-8 qui est encodé, octet par octet.
        assert_eq!(base64("é".as_bytes()), "w6k=");

        let sequence = clipboard_sequence("foobar");
        assert_eq!(sequence, "\x1b]52;c;Zm9vYmFy\x07");
    }

    #[test]
    fn le_nom_de_fichier_reste_sain() {
        // Un endpoint est souvent une URI, une exception un nom qualifié.
        assert_eq!(sanitize("/api/orders/42"), "api-orders-42");
        assert_eq!(sanitize("App\\Exception\\Boom"), "App-Exception-Boom");
        assert_eq!(sanitize("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize("///"), "");
        assert!(sanitize(&"a".repeat(100)).len() <= 40);
    }
}
